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
//! Only PostgreSQL and MySQL can be attached: they are the engines DuckDB has a
//! **core** extension for. SQL Server and MongoDB have community extensions, which
//! this session refuses to load — see `allow_community_extensions` in
//! [`super::open_duckdb_with`] — so for those two, `@import` is the whole story.

use std::fmt;

use crate::error::{Error, Result};
use crate::model::{AuthConfig, SourceConfig, SourceKind, TlsMode};

use super::{quote_identifier, quote_literal};

/// An engine DuckDB can attach through a core extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Postgres,
    MySql,
}

impl Engine {
    /// Which source kinds can be attached at all. The `None` arm is the useful
    /// half: it is what produces the error naming `@import` instead.
    pub fn of(kind: SourceKind) -> Option<Engine> {
        match kind {
            SourceKind::Postgres => Some(Engine::Postgres),
            SourceKind::MySql => Some(Engine::MySql),
            _ => None,
        }
    }

    /// The DuckDB extension, which is also the `TYPE` the ATTACH takes.
    pub fn extension(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::MySql => "mysql",
        }
    }

    /// How this engine spells Alkyon's encryption choice.
    ///
    /// The two vocabularies do not line up by name, which is exactly why this is a
    /// table and not a `to_lowercase()`. libpq's `require` encrypts *without*
    /// checking the certificate, so it is the trust-any mode, and `verify-full` is
    /// the one that validates. MySQL's `required` is the same trap.
    fn tls(self, mode: TlsMode) -> (&'static str, &'static str) {
        let value = match (self, mode) {
            (Engine::Postgres, TlsMode::Disable) => "disable",
            (Engine::Postgres, TlsMode::Prefer) => "prefer",
            (Engine::Postgres, TlsMode::Require) => "verify-full",
            (Engine::Postgres, TlsMode::TrustCertificate) => "require",
            (Engine::MySql, TlsMode::Disable) => "disabled",
            (Engine::MySql, TlsMode::Prefer) => "preferred",
            (Engine::MySql, TlsMode::Require) => "verify_identity",
            (Engine::MySql, TlsMode::TrustCertificate) => "required",
        };
        match self {
            Engine::Postgres => ("SSLMODE", value),
            Engine::MySql => ("SSL_MODE", value),
        }
    }

    /// What this engine calls the database in a secret. libpq says `dbname` in a
    /// DSN, but the secret takes `DATABASE` for both.
    fn database_key(self) -> &'static str {
        "DATABASE"
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
            "@attach {alias}: DuckDB has no core extension for a {:?} source, so it cannot be \
             attached. Import it instead — `@import {alias} = … : <sql>` — which runs your SQL \
             on the server and brings back the answer.",
            config.kind
        ))
    })?;

    let (username, password) = match &config.auth {
        AuthConfig::Password { username, password } => (username.clone(), password.clone()),
        other => {
            return Err(Error::BadRequest(format!(
                "@attach {alias}: DuckDB opens its own connection, and its {} extension takes a \
                 login and password — `{}` is not something it can use. Import the source \
                 instead: `@import` connects the way alkyon does.",
                engine.extension(),
                other.method()
            )))
        }
    };

    let (tls_key, tls_value) = engine.tls(config.tls);
    let secret_name = format!("alkyon_attach_{alias}");
    let secret = format!(
        "CREATE OR REPLACE SECRET {name} (TYPE {kind}, HOST {host}, PORT {port}, \
         {db_key} {database}, USER {user}, PASSWORD {password}, {tls_key} {tls_value});",
        name = quote_identifier(&secret_name),
        kind = engine.extension(),
        host = quote_literal(&config.host),
        port = config.port(),
        db_key = engine.database_key(),
        database = quote_literal(config.database()),
        user = quote_literal(&username),
        password = quote_literal(&password),
        tls_value = quote_literal(tls_value),
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

    /// The error has to say what to do instead, because for these two kinds
    /// `@import` is not a workaround — it is the supported path.
    #[test]
    fn an_engine_without_a_core_extension_says_so() {
        for kind in [
            SourceKind::MsSql,
            SourceKind::Mongo,
            SourceKind::Folder,
            SourceKind::Adls,
        ] {
            let error = plan("x", &config(kind, TlsMode::Prefer))
                .expect_err("not attachable")
                .to_string();
            assert!(error.contains("@import"), "{kind:?}: {error}");
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
