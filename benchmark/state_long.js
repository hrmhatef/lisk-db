const sodium = require('sodium-native');
const bunyan = require('bunyan');
const crypto = require('crypto');
const os = require('os');
const { PerformanceObserver, performance } = require('perf_hooks');

const { StateDB } = require('../index.js');

const logger = bunyan.createLogger({
    name: 'benchmark',
    streams: [
        {
            level: 'info',
            stream: process.stdout            // log INFO and above to stdout
        },
        {
            level: 'info',
            path: __dirname + '/benchmark.log'  // log ERROR and above to a file
        }
    ]
});

const getRandomBytes = length => {
    const nonce = Buffer.alloc(length);
    sodium.randombytes_buf(nonce);
    return nonce;
}

const hash = data => {
    const dataHash = crypto.createHash('sha256');
    dataHash.update(data);

    return dataHash.digest();
};


(async () => {
    let root = Buffer.alloc(0);
    let db = new StateDB('./.tmp-state');
    logger.info({ cpus: os.cpus(), arch: os.arch(), mem: os.totalmem(), ver: os.version() }, "Starting benchmark");
    const obs = new PerformanceObserver((items) => {
        for (const entry of items.getEntries()) {
            logger.info({ duration: entry.duration, name: entry.name });
        }
        // performance.clearMarks();
    });
    obs.observe({ type: 'measure' });

    for (let i = 0; i < 1000; i++) {
        db.clear();
        logger.info(`Executing ${i + 1} with root ${root.toString('hex')}`);
        performance.mark('s-start');
        for (let j = 0; j < 10000; j++) {
            db.set(getRandomBytes(36), getRandomBytes(100));
        }
        performance.mark('s-end');
        performance.mark('c-start');
        root = await db.commit(root);
        performance.mark('c-end');

        performance.measure(`Setting ${i}`, 's-start', 's-end');
        performance.measure(`Commit ${i}`, 'c-start', 'c-end');
    }

    db.close();
})()