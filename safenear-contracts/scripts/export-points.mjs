// Exports the SafeNear testnet points leaderboard to CSV for the mainnet airdrop snapshot.
// Usage: node scripts/export-points.mjs safenear.tagabukid.testnet > points.csv
// Needs Node 18+ (built-in fetch). Close the campaign first: set_points_open {"open": false}
const factory = process.argv[2];
if (!factory) { console.error("Usage: node export-points.mjs <factory-account>"); process.exit(1); }
const RPCS = ["https://rpc.testnet.fastnear.com", "https://rpc.testnet.near.org"];

async function view(method, args = {}) {
  for (const url of RPCS) {
    try {
      const r = await fetch(url, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({
        jsonrpc: "2.0", id: "x", method: "query",
        params: { request_type: "call_function", finality: "final", account_id: factory, method_name: method,
                  args_base64: Buffer.from(JSON.stringify(args)).toString("base64") } }) });
      const j = await r.json();
      if (j.error || j.result?.error) throw new Error(JSON.stringify(j.error || j.result.error));
      return JSON.parse(Buffer.from(j.result.result).toString());
    } catch (e) { var last = e; }
  }
  throw last;
}

const count = await view("get_points_count");
const rows = [];
for (let i = 0; i < count; i += 200) rows.push(...await view("get_points_page", { from_index: i, limit: 200 }));
rows.sort((a, b) => Number(b.points) - Number(a.points));

console.log("rank,testnet_account,mainnet_account,points");
rows.forEach((r, i) => console.log([i + 1, r.account_id, r.mainnet_account || "", r.points].join(",")));
console.error(`Exported ${rows.length} accounts. Accounts without a mainnet_account linked can't receive the airdrop.`);
