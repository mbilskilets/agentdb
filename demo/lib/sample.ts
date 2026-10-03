import type { Tenant } from "agentdb";

/** Creates three linked tables and a few documents to play with. */
export async function loadSample(db: Tenant) {
  await db.migrate([
    {
      op: "define_table",
      table: {
        name: "companies",
        description: "Organisations that clients belong to",
        fields: [
          { name: "name", type: "text", required: true },
          { name: "industry", type: "enum", values: ["software", "fintech", "retail", "health"], required: false },
          { name: "country", type: "text", required: false },
        ],
      },
    },
    {
      op: "define_table",
      table: {
        name: "employees",
        description: "People who work at the business",
        fields: [
          { name: "name", type: "text", required: true },
          { name: "role", type: "enum", values: ["sales", "support", "engineer", "manager"], required: false },
          { name: "salary", type: "number", required: false },
          { name: "remote", type: "bool", required: false },
          { name: "hired_at", type: "datetime", required: false },
        ],
      },
    },
    {
      op: "define_table",
      table: {
        name: "clients",
        description: "Customers and prospects",
        fields: [
          { name: "name", type: "text", required: true },
          { name: "email", type: "text", required: false },
          { name: "status", type: "enum", values: ["lead", "active", "churned"], required: false },
          { name: "revenue", type: "number", required: false },
          { name: "vip", type: "bool", required: false },
          { name: "signed_at", type: "datetime", required: false },
          { name: "company", type: "ref", table: "companies", required: false },
          { name: "owner", type: "ref", table: "employees", required: false },
        ],
      },
    },
  ]);
  const companies = [
    { name: "Northwind", industry: "software", country: "Poland" },
    { name: "Contoso", industry: "fintech", country: "USA" },
    { name: "Fabrikam", industry: "retail", country: "Germany" },
  ];
  const employees = [
    { name: "Anna Kowalska", role: "sales", salary: 9000, remote: false, hired_at: "2022-03-01" },
    { name: "Ben Carter", role: "sales", salary: 8500, remote: true, hired_at: "2023-09-15" },
    { name: "Eva Lindqvist", role: "engineer", salary: 14000, remote: true, hired_at: "2020-06-20" },
    { name: "Hugo Martin", role: "support", salary: 6200, remote: true, hired_at: "2025-11-11" },
  ];
  const clients = [
    { name: "Acme Corp", email: "hello@acme.com", status: "active", revenue: 12000, vip: true, signed_at: "2026-03-10", company: 1, owner: 1 },
    { name: "Globex", email: "hello@globex.com", status: "lead", revenue: 0, vip: false, company: 2, owner: 2 },
    { name: "Initech", status: "lead", revenue: 500, vip: false, company: 3, owner: 1 },
    { name: "Umbrella Health", email: "hello@umbrella.com", status: "active", revenue: 7500, vip: true, signed_at: "2026-10-02", company: 1, owner: 2 },
    { name: "Hooli", email: "hello@hooli.com", status: "churned", revenue: 3000, vip: false, signed_at: "2025-06-01", company: 2, owner: 2 },
    { name: "Stark Industries", email: "hello@stark.com", status: "active", revenue: 45000, vip: true, signed_at: "2026-01-20", company: 3, owner: 1 },
  ];
  for (const doc of companies) await db.insert("companies", doc);
  for (const doc of employees) await db.insert("employees", doc);
  for (const doc of clients) await db.insert("clients", doc);
  return db.describe();
}
