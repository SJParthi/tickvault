# AWS Budget Enforcement — ₹5,000/mo Hard Cap

> **Authority:** Parthiban (architect). Non-negotiable.
> **Scope:** Any file touching AWS deployment, infrastructure, Docker config, or cost-impacting changes.

## Budget: ₹5,000/month MAXIMUM (all AWS services included)

### Approved AWS Bill (verified from AWS Pricing API, ap-south-1, 2026-04-08)

| Component | Spec | Unit Price (USD) | Monthly ₹ |
|-----------|------|-----------------|-----------|
| EC2 | c7i.xlarge, on-demand, 9hr × 22 weekdays | $0.1785/hr | ₹3,004 |
| EBS | gp3, 200GB (90-day data retention) | $0.0912/GB-mo | ₹1,550 |
| Elastic IP | 1 static IP (idle ~15hr/day + weekends) | $0.005/hr idle | ₹100 |
| CloudWatch | 7 metrics + 5 alarms + 2GB logs | FREE (within free tier) | ₹0 |
| S3 | 10GB (instrument backups) | $0.025/GB-mo | ₹21 |
| SNS | ~100 SMS alerts/month (India) | $0.00278/msg | ₹25 |
| Data Transfer | ~10GB outbound | ~$0.01/GB | ₹85 |
| **TOTAL** | | | **₹4,785** |
| **Buffer** | | | **₹215** |

### Pricing Type: ON-DEMAND (not reserved, not spot)
- No upfront commitment. Pay only when instance is running.
- Instance runs 8AM-5PM IST weekdays only (EventBridge Scheduler).
- Manual start for weekends/deployments as needed.

## Mechanical Rules

1. **NEVER provision a larger instance than c7i.xlarge** without Parthiban's approval.
2. **NEVER increase EBS beyond 200GB** without Parthiban's approval.
   If 200GB fills up, use partition manager to detach old partitions (already implemented).
3. **NEVER add paid AWS services** (RDS, ElastiCache, ALB, NAT Gateway, etc.) without budget review.
4. **CloudWatch must stay within free tier**: max 10 custom metrics, 10 alarms, 5GB logs/month.
5. **S3 must stay under 50GB**. Use S3 Intelligent-Tiering for data older than 30 days.
6. **Docker memory budget for c7i.xlarge (8GB)**:
   - QuestDB: 4GB | Valkey: 1GB | Prometheus: 512MB | Grafana: 1GB
   - Alertmanager: 256MB | Traefik: 512MB | Valkey-exporter: 128MB
   - Total: 7.4GB. Remaining: 600MB for OS.
   - Jaeger, Loki, Alloy: REMOVED (saves 2.5GB RAM).

## What This Prevents

- Accidentally provisioning c7i.2xlarge (doubles EC2 cost to ₹6,008)
- EBS growing unbounded (500GB = ₹3,876/mo, blows budget)
- Adding CloudWatch Logs beyond free tier (₹0.67/GB adds up fast)
- Forgetting Elastic IP charges when instance is stopped
- Adding managed services that balloon the bill

## Instance Schedule

- **Start:** 7:45 AM IST (EventBridge cron: `cron(15 2 ? * MON-FRI *)`)
- **Stop:** 5:15 PM IST (EventBridge cron: `cron(45 11 ? * MON-FRI *)`)
- **Manual:** Start/stop via AWS Console or `aws ec2 start-instances` anytime
- **Static IP:** Elastic IP stays associated 24/7 (₹100/mo idle cost)

## Trigger

This rule activates when editing files matching:
- `deploy/docker/docker-compose.yml`
- `deploy/aws/*`
- `scripts/aws-*`
- `crates/app/src/infra.rs`
- Any file containing `c7i`, `mem_limit`, `EBS`, `gp3`, `instance_type`, `aws_region`
