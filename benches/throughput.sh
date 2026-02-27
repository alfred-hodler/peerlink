#!/bin/bash

# Check if all three mandatory parameters were provided
if [ "$#" -ne 3 ]; then
    echo "Usage: $0 <size> <count> <number_of_senders>"
    exit 1
fi

cargo build --release --benches

SIZE=$1
COUNT=$2
NUM_SENDERS=$3

PIDS=()

LATEST_BENCH=$(find ./target/release/deps -maxdepth 1 -type f -executable -name "throughput-*" -printf '%T@ %p\n' | sort -n | tail -1 | cut -f2- -d" ")

if [ -z "$LATEST_BENCH" ]; then
    echo "Error: Throughput binary not found."
    exit 1
fi

echo "Launching $NUM_SENDERS senders..."

# Trap SIGINT (Ctrl+C) to kill all children if the script is interrupted
trap 'echo "Terminating senders..."; kill "${PIDS[@]}" 2>/dev/null; exit' SIGINT

for (( i=0; i<$NUM_SENDERS; i++ ))
do
    $LATEST_BENCH sender --index "$i" &
    
    # Store PID to wait on them later
    PIDS+=($!)
done

echo "All senders launched. Waiting for completion..."

sleep 0.5 

$LATEST_BENCH sink --size $SIZE --count $COUNT --senders $NUM_SENDERS

# Wait for all background processes to finish
for pid in "${PIDS[@]}"; do
    wait "$pid"
done
