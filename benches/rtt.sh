#!/bin/bash

# parameters are size, count

BENCHMARK_SETS=(
    "1024        1000000"
    "10240       100000"
    "102400      10000"
    "1024000     1000"
)

cargo build --release --benches

SIZE=$1
COUNT=$2

PIDS=()

LATEST_BENCH=$(find ./target/release/deps -maxdepth 1 -type f -executable -name "rtt-*" -printf '%T@ %p\n' | sort -n | tail -1 | cut -f2- -d" ")

if [ -z "$LATEST_BENCH" ]; then
    echo "Error: rtt binary not found."
    exit 1
fi

echo > report.txt

# Iterate through each set of parameters
for SET in "${BENCHMARK_SETS[@]}"; do
    # Split the string into individual variables
    read -r SIZE COUNT <<< "$SET"
    
    echo "--- Running Benchmark: Size=$SIZE, Count=$COUNT ---"
    
    PIDS=()
    
    # Trap SIGINT to kill all senders if the script is interrupted
    trap 'echo "Terminating..."; kill "${PIDS[@]}" 2>/dev/null; exit' SIGINT

    echo "Launching responder..."
    $LATEST_BENCH responder &
    PIDS+=($!)

    echo "Waiting for completion..."
    sleep 0.5 

    $LATEST_BENCH initiator --size "$SIZE" --count "$COUNT" >> report.txt
    PIDS+=($!)
    
    # Wait for everything to finish before starting the next set
    for pid in "${PIDS[@]}"; do
        wait "$pid"
    done
    
    echo "Finished set. Moving to next..."
done

echo "All benchmark sets completed."
less report.txt
