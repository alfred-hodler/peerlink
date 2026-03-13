#!/bin/bash

# parameters are size, count, senders
BENCHMARK_SETS=(
    "1024        10000000    3"
    "10240       1000000     3"
    "102400      100000      3"
    "1024000     10000       3"
)

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

echo > report.txt

# Iterate through each set of parameters
for SET in "${BENCHMARK_SETS[@]}"; do
    # Split the string into individual variables
    read -r SIZE COUNT NUM_SENDERS <<< "$SET"
    
    echo "--- Running Benchmark: Size=$SIZE, Count=$COUNT, Senders=$NUM_SENDERS ---"
    
    PIDS=()
    
    # Trap SIGINT to kill all senders if the script is interrupted
    trap 'echo "Terminating senders..."; kill "${PIDS[@]}" 2>/dev/null; exit' SIGINT

    echo "Launching $NUM_SENDERS senders..."
    for (( i=0; i<$NUM_SENDERS; i++ ))
    do
        $LATEST_BENCH sender --index "$i" &
        PIDS+=($!)
    done

    echo "All senders launched. Waiting for completion..."
    sleep 0.5 

    $LATEST_BENCH sink --size "$SIZE" --count "$COUNT" --senders "$NUM_SENDERS" >> report.txt

    # Wait for all senders to finish before starting the next set
    for pid in "${PIDS[@]}"; do
        wait "$pid"
    done
    
    echo "Finished set. Moving to next..."
    echo ""
    sleep 5
done

echo "All benchmark sets completed."
less report.txt
