//! `ATTACH` a live server into the federated session, so **DuckDB plans the
//! remote read** instead of Alkyon pulling rows through itself.
//!
//! This is the counterpart to `@import`, not its replacement, and the difference
//! is worth being precise about because it decides which one to reach for.
//! Measured against the dev PostgreSQL with `pg_debug_show_queries`, here is what
//! the server actually receives:
//!
//! ```text
//! select count(*) from pg.sales.customer
//!   → SELECT NULL FROM "sales"."customer"          -- every row, then DuckDB counts
//! select name from pg.sales.customer where id = 7
//!   → SELECT "id","name" … WHERE "id" = '7'        -- projection and filter, pushed
//! select country, count(*) … group by 1
//!   → SELECT "country" FROM "sales"."customer"     -- all 250 values, grouped here
//! customer join order_line where country = 'BE'
//!   → two separate COPYs; order_line comes over whole
//! ```
//!
//! So: **projections and filters are pushed down. Aggregations and joins are
//! not.** `@attach` wins when you want to reach into a large remote table and take
//! a slice of it, or to join across engines without writing SQL per engine.
//! `@import` still wins whenever the remote engine should do the work — a
//! `group by` over a hundred million rows, a window function, anything in a
//! dialect DuckDB does not speak — because there the server runs *your* SQL
//! verbatim and only the answer travels.
//!
//! PostgreSQL and MySQL are attached through DuckDB's **core** extensions. SQL
//! Server is attached through a **community** one, which is third-party native code
//! loaded into this process — see [`community_allowed`] for the stance and the way
//! back out of it. MongoDB has a community extension too, but it is not published
//! for every DuckDB version and platform, so it is not offered; alkyon's own
//! connector already reads MongoDB, minus the pushdown.

use std::fmt;

use crate::error::{Error, Result};
use crate::model::{AuthConfig, SourceConfig, SourceKind, TlsMode};

use super::{quote_identifier, quote_literal};

/// Whether alkyon may load DuckDB **community** extensions.
///
/// On by default, because `@attach` on a SQL Server source is worth having and
/// there is no core extension for it. `ALKYON_COMMUNITY_EXTENSIONS=off` restores
/// the stance this codebase held before: nothing but DuckDB's own signed
/// extensions, ever.
///
/// The distinction is not pedantry. A core extension is built and signed by the
/// DuckDB project; a community one is third-party code, signed by the community
/// repository, that runs inside this process with everything this process can
/// reach. Worth a sentence in the guide and a line in the log, which is what it
/// gets.
pub fn community_allowed() -> bool {
    !std::env::var("ALKYON_COMMUNITY_EXTENSIONS")
        .is_ok_and(|value| matches!(value.trim().to_lowercase().as_str(), "off" | "false" | "0"))
}

/// An engine DuckDB can attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Postgres,
    MySql,
    MsSql,
}

impl Engine {
    /// Which source kinds can be attached at all. The `None` arm is the useful
    /// half: it is what produces the error naming `@import` instead.
    pub fn of(kind: SourceKind) -> Option<Engine> {
        match kind {
            SourceKind::Postgres => Some(Engine::Postgres),
            SourceKind::MySql => Some(Engine::MySql),
            SourceKind::MsSql => Some(Engine::MsSql),
            _ => None,
        }
    }

    /// The DuckDB extension, which is also the `TYPE` the ATTACH takes.
    pub fn extension(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::MySql => "mysql",
            Engine::MsSql => "mssql",
        }
    }

    /// Where the extension comes from. `Some("community")` means third-party code.
    pub fn repository(self) -> Option<&'static str> {
        match self {
            Engine::Postgres | Engine::MySql => None,
            Engine::MsSql => Some("community"),
        }
    }

    /// How this engine spells Alkyon's encryption choice, as secret parameters.
    ///
    /// The vocabularies do not line up by name, which is exactly why this is a
    /// table and not a `to_lowercase()`. libpq's `require` encrypts *without*
    /// checking the certificate, so it is the trust-any mode and `verify-full` is
    /// the one that validates; MySQL's `required` is the same trap.
    ///
    /// SQL Server is the odd one out, and it is a refusal rather than a mapping.
    /// Measured against the community extension: `USE_ENCRYPT` is the only knob it
    /// has, no secret or DSN parameter changes it, and a connection to a host whose
    /// certificate cannot match it succeeds anyway. It encrypts; it never
    /// validates. So *Require*, which promises validation, is refused instead of
    /// being quietly downgraded — that downgrade is precisely the trade this
    /// codebase refuses everywhere else.
    fn tls(self, mode: TlsMode, alias: &str) -> Result<String> {
        Ok(match self {
            Engine::Postgres => format!(
                "SSLMODE {}",
                quote_literal(match mode {
                    TlsMode::Disable => "disable",
                    TlsMode::Prefer => "prefer",
                    TlsMode::Require => "verify-full",
                    TlsMode::TrustCertificate => "require",
                })
            ),
            Engine::MySql => format!(
                "SSL_MODE {}",
                quote_literal(match mode {
                    TlsMode::Disable => "disabled",
                    TlsMode::Prefer => "preferred",
                    TlsMode::Require => "verify_identity",
                    TlsMode::TrustCertificate => "required",
                })
            ),
            Engine::MsSql => match mode {
                TlsMode::Disable => "USE_ENCRYPT false".to_owned(),
                TlsMode::Prefer | TlsMode::TrustCertificate => "USE_ENCRYPT true".to_owned(),
                TlsMode::Require => {
                    return Err(Error::BadRequest(format!(
                        "@attach {alias}: this source is set to *Require*, which means validate \
                         the certificate — and DuckDB's `mssql` extension encrypts without ever \
                         checking one. Rather than quietly hand you a weaker connection than you \
                         asked for: import it with `@import`, which connects the way alkyon does, \
                         or set the source to *Trust certificate* if that is genuinely acceptable \
                         here."
                    )))
                }
            },
        })
    }

    /// The credential, as secret parameters. Not every engine takes every method.
    fn credential(self, auth: &AuthConfig, alias: &str) -> Result<String> {
        match (self, auth) {
            (_, AuthConfig::Password { username, password }) => Ok(format!(
                "USER {}, PASSWORD {}",
                quote_literal(username),
                quote_literal(password)
            )),
            // The Entra path, and the reason SQL Server is worth attaching at all:
            // `authorise` has already turned a browser sign-in into a bearer token
            // by the time this runs, and the extension parses exactly that.
            (Engine::MsSql, AuthConfig::AadToken { token }) => {
                Ok(format!("ACCESS_TOKEN {}", quote_literal(token)))
            }
            (_, other) => Err(Error::BadRequest(format!(
                "@attach {alias}: DuckDB opens its own connection, and its {} extension cannot \
                 use `{}`. Import the source instead: `@import` connects the way alkyon does.",
                self.extension(),
                other.method()
            ))),
        }
    }
}

/// Why a kind cannot be attached, for the error that says to import it instead.
fn unattachable(kind: SourceKind) -> &'static str {
    match kind {
        // The community `mongo` extension exists, but is not published for every
        // DuckDB version and platform — and alkyon reads MongoDB itself anyway.
        SourceKind::Mongo => {
            "the community `mongo` extension is not published for this DuckDB build"
        }
        SourceKind::Folder | SourceKind::File => "it is a folder of files, not a server",
        SourceKind::Adls => "it is storage, not a server",
        _ => "DuckDB has no extension for it",
    }
}

/// One attached catalogue, as the DDL that creates it.
///
/// Held as SQL rather than as fields because the secret is in it: one place that
/// knows how to render a credential, and a `Debug` that refuses to.
pub struct Attachment {
    /// The catalogue name inside DuckDB — `pg` in `pg.sales.customer`.
    pub alias: String,
    pub engine: Engine,
    /// `CREATE SECRET …`. **Never log this.**
    secret: String,
    /// `ATTACH '' AS … (TYPE …, SECRET …, READ_ONLY)`.
    attach: String,
}

impl fmt::Debug for Attachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attachment")
            .field("alias", &self.alias)
            .field("engine", &self.engine)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl Attachment {
    /// The two statements, in the order they must run.
    pub fn statements(&self) -> [&str; 2] {
        [&self.secret, &self.attach]
    }
}

/// Turn a resolved source into the DDL that attaches it.
///
/// A secret rather than a DSN in the ATTACH string, for the same reason the Azure
/// session uses one: the credential then lives in one statement that is never
/// echoed, rather than inside a string that shows up in every error message
/// DuckDB writes about the attachment.
pub fn plan(alias: &str, config: &SourceConfig) -> Result<Attachment> {
    let engine = Engine::of(config.kind).ok_or_else(|| {
        Error::BadRequest(format!(
            "@attach {alias}: this source cannot be attached — {}. Import it instead: \
             `@import {alias} = … : <sql>` runs your SQL on the source and brings back the \
             answer.",
            unattachable(config.kind)
        ))
    })?;

    if engine.repository().is_some() && !community_allowed() {
        return Err(Error::BadRequest(format!(
            "@attach {alias}: attaching {} needs DuckDB's community `{}` extension, and \
             ALKYON_COMMUNITY_EXTENSIONS is off — community extensions are third-party code \
             running inside alkyon. Unset it to allow them, or use `@import`.",
            engine.extension(),
            engine.extension(),
        )));
    }

    let secret_name = format!("alkyon_attach_{alias}");
    let secret = format!(
        "CREATE OR REPLACE SECRET {name} (TYPE {kind}, HOST {host}, PORT {port}, \
         DATABASE {database}, {credential}, {tls});",
        name = quote_identifier(&secret_name),
        kind = engine.extension(),
        host = quote_literal(&config.host),
        port = config.port(),
        database = quote_literal(config.database()),
        credential = engine.credential(&config.auth, alias)?,
        tls = engine.tls(config.tls, alias)?,
    );

    // READ_ONLY without an opt-out. A federated buffer is for reading, and an
    // `insert` typed into one is far more likely to be a mistake than an
    // intention — while `@import` cannot write at all, so this would be the one
    // path in the whole program that mutates a production server.
    let attach = format!(
        "ATTACH '' AS {alias} (TYPE {kind}, SECRET {secret}, READ_ONLY);",
        alias = quote_identifier(alias),
        kind = engine.extension(),
        secret = quote_identifier(&secret_name),
    );

    Ok(Attachment {
        alias: alias.to_owned(),
        engine,
        secret,
        attach,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(kind: SourceKind, tls: TlsMode) -> SourceConfig {
        SourceConfig {
            id: "s".into(),
            scope: Default::default(),
            kind,
            host: "db.example.com".into(),
            port: Some(6000),
            instance: None,
            database: Some("warehouse".into()),
            auth: AuthConfig::Password {
                username: "reader".into(),
                password: "pa'ss".into(),
            },
            tls,
            path: None,
            options: Default::default(),
        }
    }

    #[test]
    fn a_postgres_source_becomes_a_secret_and_an_attach() {
        let plan = plan("pg", &config(SourceKind::Postgres, TlsMode::Require)).unwrap();
        let [secret, attach] = plan.statements();

        assert!(secret.contains("TYPE postgres"));
        assert!(secret.contains("HOST 'db.example.com'"));
        assert!(secret.contains("PORT 6000"));
        assert!(secret.contains("DATABASE 'warehouse'"));
        // Doubled, so a quote in a password cannot end the literal it sits in.
        assert!(secret.contains("PASSWORD 'pa''ss'"), "{secret}");
        // `require` would encrypt without checking the certificate, which is not
        // what alkyon's `Require` promises.
        assert!(secret.contains("SSLMODE 'verify-full'"), "{secret}");

        assert!(attach.contains("ATTACH '' AS \"pg\""));
        assert!(attach.contains("SECRET \"alkyon_attach_pg\""));
        assert!(attach.contains("READ_ONLY"));
    }

    /// The mapping is a table because the words do not mean the same thing in the
    /// two vocabularies — `require`/`required` encrypt without validating.
    #[test]
    fn each_tls_mode_keeps_its_meaning() {
        for (mode, postgres, mysql) in [
            (TlsMode::Disable, "disable", "disabled"),
            (TlsMode::Prefer, "prefer", "preferred"),
            (TlsMode::Require, "verify-full", "verify_identity"),
            (TlsMode::TrustCertificate, "require", "required"),
        ] {
            let pg = plan("a", &config(SourceKind::Postgres, mode)).unwrap();
            assert!(
                pg.statements()[0].contains(&format!("SSLMODE '{postgres}'")),
                "{mode:?} → {}",
                pg.statements()[0]
            );
            let my = plan("a", &config(SourceKind::MySql, mode)).unwrap();
            assert!(
                my.statements()[0].contains(&format!("SSL_MODE '{mysql}'")),
                "{mode:?} → {}",
                my.statements()[0]
            );
        }
    }

    /// SQL Server's extension encrypts but never validates — measured: no secret or
    /// DSN parameter changes it, and a certificate that cannot match the host is
    /// accepted anyway. *Require* promises validation, so it is refused rather than
    /// quietly downgraded to "encrypted, trusting anything".
    #[test]
    fn sql_server_refuses_require_rather_than_pretending() {
        let error = plan("ms", &config(SourceKind::MsSql, TlsMode::Require))
            .expect_err("Require cannot be honoured")
            .to_string();
        assert!(error.contains("without ever checking"), "{error}");
        assert!(error.contains("@import"), "{error}");
        assert!(error.contains("Trust certificate"), "{error}");

        for (mode, expected) in [
            (TlsMode::Disable, "USE_ENCRYPT false"),
            (TlsMode::Prefer, "USE_ENCRYPT true"),
            (TlsMode::TrustCertificate, "USE_ENCRYPT true"),
        ] {
            let plan = plan("ms", &config(SourceKind::MsSql, mode)).unwrap();
            assert!(plan.statements()[0].contains(expected), "{mode:?}");
        }
    }

    /// The reason SQL Server is worth attaching: an Entra sign-in has already become
    /// a bearer token by the time this runs, and the extension parses exactly that.
    #[test]
    fn an_entra_token_becomes_an_access_token() {
        let mut config = config(SourceKind::MsSql, TlsMode::TrustCertificate);
        config.auth = AuthConfig::AadToken {
            token: "header.payload.signature".into(),
        };
        let plan = plan("ms", &config).unwrap();
        assert!(
            plan.statements()[0].contains("ACCESS_TOKEN 'header.payload.signature'"),
            "{}",
            plan.statements()[0]
        );
        // And it must not be reachable through the formatter used in log lines.
        assert!(!format!("{plan:?}").contains("payload"));
    }

    /// A token is a SQL Server thing. PostgreSQL's extension has no such parameter,
    /// so offering it would be inventing one.
    #[test]
    fn a_token_is_not_offered_to_an_engine_that_has_no_use_for_it() {
        let mut config = config(SourceKind::Postgres, TlsMode::Prefer);
        config.auth = AuthConfig::AadToken {
            token: "a.b.c".into(),
        };
        let error = plan("pg", &config).expect_err("no such parameter").to_string();
        assert!(error.contains("aad_token"), "{error}");
        assert!(error.contains("@import"), "{error}");
    }

    /// The error has to say what to do instead, because for these kinds `@import`
    /// is not a workaround — it is the supported path.
    #[test]
    fn a_kind_that_cannot_be_attached_says_why_and_what_instead() {
        for (kind, reason) in [
            (SourceKind::Mongo, "not published"),
            (SourceKind::Folder, "not a server"),
            (SourceKind::File, "not a server"),
            (SourceKind::Adls, "not a server"),
        ] {
            let error = plan("x", &config(kind, TlsMode::Prefer))
                .expect_err("not attachable")
                .to_string();
            assert!(error.contains("@import"), "{kind:?}: {error}");
            assert!(error.contains(reason), "{kind:?}: {error}");
        }
    }

    #[test]
    fn a_credential_duckdb_cannot_use_is_refused_before_it_is_tried() {
        let mut config = config(SourceKind::Postgres, TlsMode::Prefer);
        config.auth = AuthConfig::Integrated;
        let error = plan("pg", &config).expect_err("integrated").to_string();
        assert!(error.contains("integrated"), "{error}");
        assert!(error.contains("@import"), "{error}");
    }

    /// Which extensions are third-party. The list is small and worth being explicit
    /// about, since it is the difference between DuckDB's own signed code and
    /// somebody else's running in this process.
    #[test]
    fn only_sql_server_comes_from_the_community() {
        assert_eq!(Engine::Postgres.repository(), None);
        assert_eq!(Engine::MySql.repository(), None);
        assert_eq!(Engine::MsSql.repository(), Some("community"));
    }

    /// The credential must not be reachable through the formatter that gets used
    /// in log lines and error messages.
    #[test]
    fn debug_does_not_leak_the_password() {
        let plan = plan("pg", &config(SourceKind::Postgres, TlsMode::Prefer)).unwrap();
        let shown = format!("{plan:?}");
        assert!(!shown.contains("pa'ss"), "{shown}");
        assert!(shown.contains("redacted"), "{shown}");
    }
}
