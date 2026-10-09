// Deploys res/safenear_factory.wasm to NEAR_ACCOUNT_ID on testnet and initializes it.
// Any problem is shown as a plain-language error on the GitHub Actions Summary page.
import fs from "node:fs";
import nearAPI from "near-api-js";
const { connect, keyStores, KeyPair } = nearAPI;

const fail = (title, msg) => {
  console.log(`::error title=${title}::${msg}`);
  console.error(`\n${title}: ${msg}\n`);
  process.exit(1);
};
const note = (msg) => console.log(`::notice title=SafeNear deploy::${msg}`);

const accountId = (process.env.NEAR_ACCOUNT_ID || "").trim();
const privateKey = (process.env.NEAR_PRIVATE_KEY || "").trim();
const owner = (process.env.NEAR_OWNER_ID || "").trim() || accountId;

// 1. Secrets present?
if (!accountId) fail("Missing NEAR_ACCOUNT_ID", "Add the secret NEAR_ACCOUNT_ID with your wallet name, for example testnettestes.testnet");
if (!privateKey) fail("Missing NEAR_PRIVATE_KEY", "Add the secret NEAR_PRIVATE_KEY with your wallet's private key (starts with ed25519:)");
if (!accountId.endsWith(".testnet")) fail("Wrong account name", `NEAR_ACCOUNT_ID is "${accountId}". It must be a testnet account ending in .testnet`);

// 2. Key format
if (!privateKey.startsWith("ed25519:")) fail("Wrong key format", "NEAR_PRIVATE_KEY must start with ed25519: (copy the whole key, including ed25519:)");
const keyBody = privateKey.slice(8);
if (keyBody.length < 70) fail("That is the PUBLIC key", `NEAR_PRIVATE_KEY is only ${keyBody.length} characters after ed25519:. That's the public key. Export the PRIVATE key from your wallet (it's about 88 characters).`);
let keyPair;
try { keyPair = KeyPair.fromString(privateKey); }
catch { fail("Key can't be read", "NEAR_PRIVATE_KEY isn't a valid key. Copy it again from your wallet with no spaces or line breaks."); }
const publicKey = keyPair.getPublicKey().toString();

// 3. Connect
const keyStore = new keyStores.InMemoryKeyStore();
await keyStore.setKey("testnet", accountId, keyPair);
let near;
for (const nodeUrl of ["https://rpc.testnet.fastnear.com", "https://rpc.testnet.near.org", "https://test.rpc.fastnear.com"]) {
  try { near = await connect({ networkId: "testnet", nodeUrl, keyStore }); await near.connection.provider.status(); break; }
  catch { near = null; }
}
if (!near) fail("Can't reach NEAR testnet", "All testnet RPC servers failed. Re-run the workflow in a few minutes.");
const account = await near.account(accountId);

// 4. Account exists + balance
let state;
try { state = await account.state(); }
catch { fail("Account not found", `"${accountId}" doesn't exist on testnet. Check the spelling in NEAR_ACCOUNT_ID (copy it from your wallet).`); }
const balance = Number(BigInt(state.amount) / 10n ** 21n) / 1000;

// 5. Key belongs to this account?
const keys = await near.connection.provider.query({ request_type: "view_access_key_list", finality: "final", account_id: accountId });
const match = keys.keys.find((k) => k.public_key === publicKey);
if (!match) fail("Key doesn't match the account", `NEAR_PRIVATE_KEY belongs to a different wallet than ${accountId}. Export the private key from the ${accountId} wallet itself.`);
if (match.access_key.permission !== "FullAccess") fail("Key has limited access", "This key can only call some contracts. Export the wallet's FULL ACCESS private key instead.");

// 6. Enough NEAR for the code storage?
const wasm = fs.readFileSync("res/safenear_factory.wasm");
const needed = (wasm.length / 100_000) + 1; // 1 NEAR per 100 KB plus buffer
if (balance < needed) fail("Not enough testnet NEAR", `${accountId} has ${balance.toFixed(2)} NEAR but deploying needs about ${needed.toFixed(1)} NEAR. Get more from near-faucet.io and re-run.`);
console.log(`Account ${accountId}: ${balance.toFixed(3)} NEAR, key OK. Deploying ${(wasm.length / 1024).toFixed(0)} KB...`);

// 7. Deploy + init
const NEAR = (n) => (BigInt(Math.round(n * 1000)) * 10n ** 21n).toString();
const initArgs = {
  owner,
  creation_fee: NEAR(5),
  token_account_balance: NEAR(4.5),
  graduation_threshold: NEAR(5),
  virtual_near: NEAR(2),
  ref_contract: "ref-finance-101.testnet",
  wrap_contract: "wrap.testnet",
};
try {
  const d = await account.deployContract(wasm);
  console.log("Deployed. Tx:", d.transaction.hash);
} catch (e) {
  fail("Deploy transaction failed", String(e?.message || e).replace(/\s+/g, " ").slice(0, 300));
}
try {
  const r = await account.functionCall({ contractId: accountId, methodName: "new", args: initArgs, gas: "100000000000000" });
  console.log("Initialized. Tx:", r.transaction.hash);
} catch (e) {
  const m = String(e?.message || e);
  if (/already been initialized|already initialized/i.test(m)) console.log("Already initialized, code updated only.");
  else fail("Setup (init) failed", m.replace(/\s+/g, " ").slice(0, 300));
}

note(`Factory is live at ${accountId}. Open the site with ?factory=${accountId}`);
console.log(`\nDone! Open the site with:  safenear-launchpad.html?factory=${accountId}`);
