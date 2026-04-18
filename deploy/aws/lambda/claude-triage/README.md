# claude-triage Lambda

Phase 8.2 of `.claude/plans/active-plan.md`.

## Flow

```
CloudWatch alarm fires
        ↓
SNS topic tv_alerts
        ↓
claude-triage Lambda (this)
        ↓
SSM RunCommand on EC2 instance
        ↓
tmux send-keys -t claude-triage "<prompt>"
        ↓
Long-running claude CLI reads the prompt,
applies .claude/triage/error-rules.yaml,
either auto-fixes / silences / escalates.
```

## Activation

Gated behind the Terraform variable `enable_claude_triage_lambda`.
Defaults to **false** — the full stack (IAM role, Lambda, SNS
subscription) only provisions when the operator opts in.

To enable:
```bash
cd deploy/aws/terraform
terraform apply -var='enable_claude_triage_lambda=true'
```

Prerequisite on the EC2 instance:
```bash
# Pre-create the long-running tmux pane that the Lambda targets.
tmux new-session -d -s claude-triage 'claude code --dangerous-run=false'
```

## Environment variables (injected by Terraform)

| Var | Source | Purpose |
|---|---|---|
| EC2_INSTANCE_ID | `aws_instance.tv.id` | Target instance for SSM command |
| TRIAGE_COMMAND_TIMEOUT | hardcoded 180s | Max seconds per triage run |
| CLAUDE_BINARY_PATH | `/usr/local/bin/claude` | Path to claude CLI on EC2 |
| TV_LOG_GROUP | `aws_cloudwatch_log_group.tv_app.name` | CloudWatch log sink for command output |

## Cost

- Lambda invocations: well inside AWS free tier (<100/day expected)
- SSM RunCommand: free
- CloudWatch Logs: 1-2 KB per invocation, inside the 5 GB free tier

## Local self-test

The handler is pure Python + boto3. Import it and call `_parse_sns_alarm`
directly to verify event-parsing logic:

```python
from handler import _parse_sns_alarm, _build_claude_prompt
event = {"Records": [{"Sns": {"Message": json.dumps({
    "AlarmName": "QuestDbDown",
    "NewStateValue": "ALARM",
    "NewStateReason": "liveness check failed",
    "Trigger": {"MetricName": "tv_questdb_alive", "Threshold": 1.0},
    "StateChangeTime": "2026-04-18T09:45:00Z",
})}}]}
alarm = _parse_sns_alarm(event)
assert alarm["alarm_name"] == "QuestDbDown"
print(_build_claude_prompt(alarm))
```
