#!/bin/bash

set -euo pipefail

cargo b --release
cp ./target/release/hurlin ./arch-pkg/hurlin
