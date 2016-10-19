#! /usr/bin/env bash

trap "trap - SIGTERM && kill -- -$$" SIGINT SIGTERM EXIT
export RUSTFLAGS=--error-format=json
export RUST_BACKTRACE=1
export RUST_LOG=sucredb=debug
SLEEP=2
cargo build
./target/debug/sucredb -d n1 -l 127.0.0.1:6379 -f 127.0.0.1:16379 --init > log1.txt 2>&1  &
echo "WAITING $SLEEP"
sleep $SLEEP
./target/debug/sucredb -d n2 -l 127.0.0.1:6378 -f 127.0.0.1:16378 > log2.txt 2>&1  &

tail -f log1.txt log2.txt