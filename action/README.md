# Oven GitHub Action

Run oven agent pipelines on labeled GitHub issues.

## Usage

```yaml
name: Oven Pipeline
on:
  issues:
    types: [labeled]
  workflow_dispatch:
    inputs:
      issue-number:
        description: 'Issue number to process'
        required: true

concurrency:
  group: oven-${{ github.event.issue.number || github.event.inputs.issue-number }}
  cancel-in-progress: false

permissions:
  contents: write
  issues: write
  pull-requests: write

jobs:
  run:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: actions/create-github-app-token@v1
        id: app-token
        with:
          app-id: ${{ vars.APP_ID }}
          private-key: ${{ secrets.APP_PRIVATE_KEY }}

      - uses: clayharmon/oven-cli/action@main
        with:
          anthropic-api-key: ${{ secrets.ANTHROPIC_API_KEY }}
          github-token: ${{ steps.app-token.outputs.token }}
          auto-merge: 'true'
          cost-budget: '15.0'
```

## Inputs

| Input | Required | Default | Description |
|-------|----------|---------|-------------|
| `anthropic-api-key` | yes | | Anthropic API key |
| `github-token` | yes | | GitHub token (from github-app-token action or GITHUB_TOKEN) |
| `oven-version` | no | `latest` | Version of oven to install |
| `auto-merge` | no | `false` | Auto-merge PRs after pipeline completes |
| `max-parallel` | no | `2` | Maximum parallel pipelines |
| `cost-budget` | no | `10.0` | Maximum cost in USD per issue |

## Outputs

| Output | Description |
|--------|-------------|
| `run-id` | The oven run ID (8 hex chars) |
| `status` | Pipeline result: complete, failed, or cancelled |
| `cost` | Total cost in USD |
| `pr-number` | Pull request number created by the pipeline |

## How it works

1. Triggers when an issue is labeled with `o-ready` (or via workflow_dispatch)
2. Installs the oven CLI and Claude CLI
3. Runs `oven on <issue-number>` to start the pipeline
4. Posts a summary comment on the issue with results
5. Sets outputs for downstream workflow steps

Per-issue concurrency groups prevent duplicate runs for the same issue.
