// Deploys res/safenear_factory.wasm to NEAR_ACCOUNT_ID on testnet and initializes it.
// Run by the GitHub Actions workflow. Secrets: NEAR_ACCOUNT_ID, NEAR_PRIVATE_KEY, optional NEAR_OWNER_ID.
import fs from "node:fs";
import nearAPI from "near-api-js";
const { connect, keyStores, KeyPair } = nearAPI;

const accountId = process.env.NEAR_ACCOUNT_ID;
const privateKey = process.env.NEAR_PRIVATE_KEY;
const owner = process.env.NEAR_OWNER_ID || accountId;
if (!accountId || !privateKey) {
  console.error("Missing secrets: add NEAR_ACCOUNT_ID and NEAR_PRIVATE_KEY in Settings > Secrets and variables > Actions.");
  process.exit(1);
}

const NEAR = (n) => (BigInt(Math.round(n * 1000)) * 10n ** 21n).toString();
const initArgs = {
  owner,
  creation_fee: NEAR(5),            // creator pays 5 NEAR
  token_account_balance: NEAR(4.5), // 4.5 NEAR goes to the new token account
  graduation_threshold: NEAR(5),    // curve graduates at 5 NEAR on testnet
  virtual_near: NEAR(2),
  ref_contract: "ref-finance-101.testnet",
  wrap_contract: "wrap.testnet",
};

const keyStore = new keyStores.InMemoryKeyStore();
await keyStore.setKey("testnet", accountId, KeyPair.fromString(privateKey));
const near = await connect({ networkId: "testnet", nodeUrl: "https://rpc.testnet.fastnear.com", keyStore });
const account = await near.account(accountId);

const { amount } = await account.state();
console.log(`Account ${accountId} balance: ${(Number(BigInt(amount) / 10n ** 21n) / 1000).toFixed(3)} NEAR`);

const wasm = fs.readFileSync("res/safenear_factory.wasm");
console.log(`Deploying factory (${(wasm.length / 1024).toFixed(0)} KB)...`);
const d = await account.deployContract(wasm);
console.log("Deployed. Tx:", d.transaction.hash);

try {
  const r = await account.functionCall({ contractId: accountId, methodName: "new", args: initArgs, gas: "100000000000000" });
  console.log("Initialized. Tx:", r.transaction.hash);
} catch (e) {
  if (String(e).includes("already been initialized")) console.log("Already initialized, code updated only.");
  else throw e;
}

console.log(`\nDone! Open the site with:  safenear-launchpad.html?factory=${accountId}`);
