// Check slots 0-15 to find where reserve0, reserve1, blockTs are stored
const RPC = 'http://127.0.0.1:8545';
const PAIR = '0x737f1cab9cd97c40bbe4d59c85b0d2c1fdbaa37d';

// From getReserves() we know:
//   r0 = 53104695875920807788    (WETH.e, ~53.1 whole)
//   r1 = 11716412100795287177237 (WAVAX, ~11716 whole)
const TARGET_R0 = 53104695875920807788n;
const TARGET_R1 = 11716412100795287177237n;

async function rpc(method, params) {
  const r = await fetch(RPC, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
  });
  return (await r.json()).result;
}

for (let slot = 0; slot <= 20; slot++) {
  const s = await rpc('eth_getStorageAt', [PAIR, '0x' + slot.toString(16), 'latest']);
  const val = BigInt(s);
  let note = '';
  if (val === TARGET_R0) note = '  ← reserve0 EXACT MATCH';
  if (val === TARGET_R1) note = '  ← reserve1 EXACT MATCH';
  // Also check lower 112 bits
  const lo = val & ((1n<<112n)-1n);
  if (lo === TARGET_R0 && val !== TARGET_R0) note = '  ← reserve0 in lower 112 bits';
  if (lo === TARGET_R1) note = '  ← reserve1 in lower 112 bits';
  console.log(`slot ${String(slot).padStart(2)}: ${s}${note}`);
}
