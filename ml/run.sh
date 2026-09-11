#!/usr/bin/env bash
set -e
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DIR/.." && pwd)"

echo "Starting Real-Time Online Load Forecaster..."
uv run --project "$DIR" python "$DIR/predictor.py" "$@"
