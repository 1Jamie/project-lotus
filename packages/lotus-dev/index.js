const binLotus = require('./bin/lotus.js');

module.exports = {
    cli: require.resolve('./bin/lotus.js'),
    ...binLotus
};
