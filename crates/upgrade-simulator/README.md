# upgrade-simulator
This is a testing framework that allows running through upgrades of a real rollup binary using sov-rollup-manager.

It takes as input a TestCase, which takes a git repo of a rollup and a set of commit hashes corresponding to each version; builds binaries for every version of the rollup, if not already cached; and then runs the rollup manager using a generated upgrade config corresponding to these versions.
