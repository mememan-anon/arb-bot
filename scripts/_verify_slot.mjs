const RPC = 'http://127.0.0.1:8545';
const PAIR = '0x737f1cab9cd97c40bbe4d59c85b0d2c1fdbaa37d';

async function rpc(method, params) {
  const r = await fetch(RPC, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
  });
  return (await r.json()).result;
}

const slot8 = await rpc('eth_getStorageAt', [PAIR, '0x8', 'latest']);
const reserves = await rpc('eth_call', [{ to: PAIR, data: '0x0902f1ac' }, 'latest']);

console.log('slot8:     ', slot8);
console.log('getReserves:', reserves);

// Decode slot8
const big = BigInt(slot8);
const MASK112 = (1n << 112n) - 1n;
const r0 = big & MASK112;
const r1 = (big >> 112n) & MASK112;
const ts = (big >> 224n);
console.log('  token0 reserve:', r0.toString());
console.log('  token1 reserve:', r1.toString());
console.log('  blockTs:        ', ts.toString());

// getReserves() returns: uint112 r0, uint112 r1, uint32 ts (ABI-encoded)
// Each is padded to 32 bytes in the raw hex result
if (reserves && reserves.length === 194) { // 0x + 3×64 hex chars
  const hex = reserves.slice(2); // strip 0x
  const gr0 = BigInt('0x' + hex.slice(0, 64));
  const gr1 = BigInt('0x' + hex.slice(64, 128));
  const gts = BigInt('0x' + hex.slice(128, 192));
  console.log('getReserves decoded:');
  console.log('  r0:', gr0.toString());
  console.log('  r1:', gr1.toString());
  console.log('  ts:', gts.toString());
}
