const http = require('http');

const pairs = [
    ['0xdb0ef0808b53219beed7ff5dca8da99d009d69a0', 'USDT/WETH.e'],
    ['0x921ca54e1d32008c25b352bb75aa00593288f1b3', 'WETH.e/BTC.b'],
    ['0x1abe428146795bc754170af24cfd78663f257d29', 'BTC.b/USDT'],
    ['0x495b296c3fc52283fd9565b421386d36f628d55e', 'pair-0x495b'],
    ['0x0e5aad7522acf208c5f691d3f20af0c26d1d669a', 'WETH.e/WAVAX'],
];

const factory = '0xfe926062fb99ca5653080d6c14fe945ad68c265c';

function call(to, data) {
    return new Promise((resolve, reject) => {
        const body = JSON.stringify({
            jsonrpc: '2.0', method: 'eth_call',
            params: [{ to, data }, 'latest'], id: 1
        });
        const req = http.request({
            host: '127.0.0.1', port: 8545, method: 'POST',
            headers: { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(body) }
        }, res => {
            let d = '';
            res.on('data', c => d += c);
            res.on('end', () => resolve(JSON.parse(d).result));
        });
        req.on('error', reject);
        req.write(body);
        req.end();
    });
}

async function main() {
    for (const [addr, name] of pairs) {
        const padded = addr.slice(2).toLowerCase().padStart(64, '0');
        const data = '0xcc56b2c5' + padded + '0'.repeat(64);
        try {
            const res = await call(factory, data);
            const fee = parseInt(res, 16);
            console.log(`${name} (${addr.slice(0,10)}): getFee = ${fee} bps`);
        } catch(e) {
            console.log(`${name}: ERROR ${e.message}`);
        }
    }
}

main();
