# Goose Benchmark Scripts

## Hickory dev workstation release

Hickory dev workstations install a pinned Goose fork binary from GitHub
Releases. They do not clone or compile Goose during an upgrade.

After testing a pushed commit, publish its Linux x86-64 binary from a clean
checkout:

```bash
scripts/release-dev-workstation.sh
```

The script uses the CLI feature profile tested by Kory Code. It excludes local
inference, which keeps the workstation binary independent of libclang. The
script creates `dev-workstation-<full-git-sha>` in `zaneclaes/goose`, uploads
`goose-x86_64-unknown-linux-gnu.tar.gz`, and uploads its SHA-256 file.

Re-running the script succeeds only when the existing release has the same
checksum. Copy the printed revision and checksum into Hickory's Goose pin
files. Keep source builds for development and testing.

This directory contains scripts for running and analyzing Goose benchmarks.

## run-benchmarks.sh

This script runs Goose benchmarks across multiple provider:model pairs and analyzes the results.

### Prerequisites

- Goose CLI must be built or installed
- `jq` command-line tool for JSON processing (optional, but recommended for result analysis)

### Usage

```bash
./scripts/run-benchmarks.sh [options]
```

#### Options

- `-p, --provider-models`: Comma-separated list of provider:model pairs (e.g., 'openai:gpt-4o,anthropic:claude-sonnet-4')
- `-s, --suites`: Comma-separated list of benchmark suites to run (e.g., 'core,small_models')
- `-o, --output-dir`: Directory to store benchmark results (default: './benchmark-results')
- `-d, --debug`: Use debug build instead of release build
- `-h, --help`: Show help message

#### Examples

```bash
# Run with release build (default)
./scripts/run-benchmarks.sh --provider-models 'openai:gpt-4o,anthropic:claude-sonnet-4' --suites 'core,small_models'

# Run with debug build
./scripts/run-benchmarks.sh --provider-models 'openai:gpt-4o' --suites 'core' --debug
```

### How It Works

The script:
1. Parses the provider:model pairs and benchmark suites
2. Determines whether to use the debug or release binary
3. For each provider:model pair:
   - Sets the `GOOSE_PROVIDER` and `GOOSE_MODEL` environment variables
   - Runs the benchmark with the specified suites
   - Analyzes the results for failures
4. Generates a summary of all benchmark runs

### Output

The script creates the following files in the output directory:

- `summary.md`: A summary of all benchmark results
- `{provider}-{model}.json`: Raw JSON output from each benchmark run
- `{provider}-{model}-analysis.txt`: Analysis of each benchmark run

### Exit Codes

- `0`: All benchmarks completed successfully
- `1`: One or more benchmarks failed

## parse-benchmark-results.sh

This script analyzes a single benchmark JSON result file and identifies any failures.

### Usage

```bash
./scripts/parse-benchmark-results.sh path/to/benchmark-results.json
```

### Output

The script outputs an analysis of the benchmark results to stdout, including:

- Basic information about the benchmark run
- Results for each evaluation in each suite
- Summary of passed and failed metrics

### Exit Codes

- `0`: All metrics passed successfully
- `1`: One or more metrics failed
