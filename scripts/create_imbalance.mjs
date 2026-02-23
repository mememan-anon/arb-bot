/**
 * create_imbalance.mjs
 *
 * Shifts one Blackhole V2 pair's reserves via anvil_setStorageAt to create
 * a price discrepancy the arb bot can exploit.
 *
 * Target: 0x737f1cab9cd97c40bbe4d59c85b0d2c1fdbaa37d  (WETH.e/WAVAX, fee=6 bps)
 *
 * Blackhole V2 storage layout (separate, non-packed slots — confirmed by scan):
 *   slot 8  = reserve0 (WETH.e)      full 256-bit slot
 *   slot 9  = reserve1 (WAVAX)       full 256-bit slot
 *   slot 10 = blockTimestampLast     full 256-bit slot
 *
 * We REDUCE reserve0 (WETH.e) by 20%.
 * Effect: pool has less WETH.e → each WETH.e sold here yields MORE WAVAX than market.
 * Profitable 2-hop path:
 *   WAVAX → WETH.e at another pool (fair price)
 *   WETH.e → WAVAX at THIS pool    (inflated return)
 */

// No external dependencies — uses built-in fetch (Node 18+)

const RPC = 'http://127.0.0.1:8545';
const PAIR = '0x737f1cab9cd97c40bbe4d59c85b0d2c1fdbaa37d';

async function rpc(method, params, id = 1) {
  const r = await fetch(RPC, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id, method, params }),
  });
  const j = await r.json();
  if (j.error) throw new Error(JSON.stringify(j.error));
  return j.result;
}

async function main() {
  // 1. Read current reserves from contract
  const raw = await rpc('eth_call', [{ to: PAIR, data: '0x0902f1ac' }, 'latest'], 0);
  const hex = raw.slice(2);
  const r0 = BigInt('0x' + hex.slice(0, 64));
  const r1 = BigInt('0x' + hex.slice(64, 128));
  const ts = BigInt('0x' + hex.slice(128, 192));
  console.log(`getReserves(): r0(WETH.e)=${r0}  r1(WAVAX)=${r1}  ts=${ts}`);
  console.log(`  ~${(r0 / 10n**18n).toString()} WETH.e  /  ~${(r1 / 10n**18n).toString()} WAVAX`);

  // 2. New reserve0 = 80% of current (20% artificial reduction)
  const newR0 = (r0 * 80n) / 100n;
  const newR0Hex = '0x' + newR0.toString(16).padStart(64, '0');
  console.log(`\nNew reserve0 (80%): ${newR0}  (~${(newR0 / 10n**18n).toString()} WETH.e)`);

  // 3. Write to slot 8
  const writeRes = await rpc('anvil_setStorageAt', [PAIR, '0x8', newR0Hex], 1);
  console.log(`anvil_setStorageAt slot8: ${writeRes}`);

  // 4. Verify
  const readBack = await rpc('eth_getStorageAt', [PAIR, '0x8', 'latest'], 2);
  console.log(`Verified slot8: ${readBack}`);
  if (BigInt(readBack) !== newR0) {
    console.error('❌ Write verification FAILED');
    process.exit(1);
  }

  // 5. Mine one block so the bot receives a newBlock event and re-reads reserves
  await rpc('anvil_mine', ['0x1'], 3);
  console.log('\n✅ Imbalance created and block mined.');
  console.log(`   r0 before: ${r0}  (${(r0 / 10n**18n)} WETH.e)`);
  console.log(`   r0 after:  ${newR0}  (${(newR0 / 10n**18n)} WETH.e) — 20% lower`);
  console.log('\n   Now start (or the running bot will pick it up on the next block).');
}

main().catch(e => { console.error('Error:', e.message); process.exit(1); });
