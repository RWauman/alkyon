// Demo data for Alkyon's MongoDB source.
//
// Run by the image's init hook on a first start, against the database named by
// MONGO_INITDB_DATABASE.
//
// Deliberately *document*-shaped rather than a table transcribed into JSON. The
// interesting questions a MongoDB source has to answer are the ones a relational
// seed cannot ask:
//
//   - nested documents and arrays, which a grid has to render as something
//   - fields that are absent from some documents rather than null in all of them
//   - the same field holding different types in different documents
//   - BSON types with no SQL equivalent: ObjectId, Decimal128, Binary
//
// The row counts match the other engines' seeds — 250 customers, 1 500 order
// lines — so a test can read the same numbers whichever source it points at.

const COUNTRIES = ["BE", "FR", "NL", "DE", null];
const TIERS = ["gold", "silver", "bronze"];
const SKUS = Array.from({ length: 39 }, (_, n) => `SKU-${String(n + 1).padStart(4, "0")}`);

const start = new Date("2022-01-01T00:00:00Z");
const day = 24 * 60 * 60 * 1000;

const customers = [];
for (let n = 1; n <= 250; n += 1) {
  const document = {
    _id: n,
    name: `Customer ${n}`,
    country: COUNTRIES[n % COUNTRIES.length],
    tier: TIERS[n % TIERS.length],
    // Money as Decimal128, which is what it is for — a double would lose cents.
    credit: Decimal128.fromString((n * 13.37).toFixed(2)),
    signed_on: new Date(start.getTime() + (n % 400) * day),
    // Nested, and deeper on some documents than others.
    address: {
      city: `City ${n % 40}`,
      postcode: `${1000 + (n % 8999)}`,
      ...(n % 5 === 0 ? { region: { code: `R${n % 12}`, name: `Region ${n % 12}` } } : {}),
    },
    tags: TIERS.slice(0, 1 + (n % 3)),
  };
  // Absent, not null: a third of these documents simply have no such field, which
  // is the thing a fixed set of columns cannot express.
  if (n % 3 === 0) {
    document.loyalty = { points: n * 7, since: new Date(start.getTime() + n * day) };
  }
  customers.push(document);
}

const orders = [];
for (let n = 0; n < 1500; n += 1) {
  orders.push({
    order_id: 1000 + n,
    customer_id: 1 + (n % 250),
    sku: SKUS[n % SKUS.length],
    quantity: 1 + (n % 7),
    unit_price: Decimal128.fromString((9.99 + (n % 40) + (n % 3) / 4).toFixed(2)),
    ordered_at: new Date(start.getTime() + n * 7 * 60 * 60 * 1000),
    // Missing on a quarter of them rather than false: "not shipped" and "we did
    // not record it" are different facts, and Mongo lets you say so.
    ...(n % 4 === 0 ? {} : { shipped: true }),
    lines: [
      { sku: SKUS[n % SKUS.length], quantity: 1 + (n % 7) },
      ...(n % 6 === 0 ? [{ sku: SKUS[(n + 1) % SKUS.length], quantity: 1 }] : []),
    ],
  });
}

// The awkward one, on purpose: one field, four types, and one document where it
// is an array. Any reader that decides a column's type from the first document
// gets this wrong, which is why it is here.
const mixed = [
  { _id: 1, value: 42, note: "an integer" },
  { _id: 2, value: "forty-two", note: "a string" },
  { _id: 3, value: 42.5, note: "a double" },
  { _id: 4, value: { amount: 42, currency: "EUR" }, note: "a document" },
  { _id: 5, value: [1, 2, 3], note: "an array" },
  { _id: 6, note: "absent" },
];

db.customer.insertMany(customers);
db.order_line.insertMany(orders);
db.awkward.insertMany(mixed);

// An index, so a source can show something other than the default one, and a
// view, so the explorer has both kinds to list.
db.order_line.createIndex({ customer_id: 1, ordered_at: -1 });
db.createView("order_value", "order_line", [
  {
    $project: {
      order_id: 1,
      customer_id: 1,
      value: { $multiply: ["$quantity", { $toDouble: "$unit_price" }] },
    },
  },
]);

print(
  `seeded ${db.customer.countDocuments()} customers, ` +
    `${db.order_line.countDocuments()} order lines, ` +
    `${db.awkward.countDocuments()} awkward documents`
);
