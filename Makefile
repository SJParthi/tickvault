# =============================================================================
# tickvault — Development & Operations Makefile
# =============================================================================
# Usage: make <target>
# Run `make help` to see all available targets.
# =============================================================================

.PHONY: help run run-supervised stop build test check fmt clippy clean \
        docker-up docker-down docker-restart docker-status docker-logs questdb-init \
        health status open grafana questdb jaeger prometheus traefik alloy loki \
        obs obs-verify obs-restart obs-open \
        logs app-pid \
        audit coverage bench geiger typos quality doc bootstrap scoped-check full-qa \
        dispatch dispatch-readonly dispatch-status dispatch-logs dispatch-check dispatch-audit \

# ---- Configuration ----
APP_NAME       := tickvault
APP_PORT       := 3001
DOCKER_DIR     := deploy/docker
COMPOSE_FILE   := $(DOCKER_DIR)/docker-compose.yml

# ---- Help ----
help: ## Show this help
	@echo ""
	@echo "  tickvault — Development & Operations"
	@echo "  ════════════════════════════════════════════"
	@echo ""
	@echo "  APPLICATION:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | grep -E '(run|stop|build|health|status|logs|open)' | awk 'BEGIN {FS = ":.*?## "}; {printf "    \033[36m%-20s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "  QUALITY:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | grep -E '(test|check|fmt|clippy|clean|audit|coverage|bench|geiger|typos|quality|doc)' | awk 'BEGIN {FS = ":.*?## "}; {printf "    \033[36m%-20s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "  DOCKER:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | grep -E '(docker)' | awk 'BEGIN {FS = ":.*?## "}; {printf "    \033[36m%-20s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "  OBSERVABILITY:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | grep -E '(obs)' | awk 'BEGIN {FS = ":.*?## "}; {printf "    \033[36m%-20s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "  MONITORING:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | grep -E '(grafana|questdb|jaeger|prometheus|traefik|alloy|loki)' | awk 'BEGIN {FS = ":.*?## "}; {printf "    \033[36m%-20s\033[0m %s\n", $$1, $$2}'
	@echo ""

# =============================================================================
# APPLICATION
# =============================================================================

run: ## Run app in dev mode (pretty logs, localhost config)
	@echo "🚀 Starting $(APP_NAME)..."
	@./scripts/ensure-ready.sh
	@cargo run

stop: ## Stop running app
	@echo "🛑 Stopping $(APP_NAME)..."
	@-pkill -f "target/debug/$(APP_NAME)" 2>/dev/null && echo "  Stopped." || echo "  Not running."

restart: stop ## Restart app (stop + run)
	@sleep 1
	@$(MAKE) run

run-supervised: docker-up ## Supervised run: restarts on crash up to 5 times with exponential backoff
	@echo "Starting $(APP_NAME) with crash supervision (max 5 restarts)..."
	@attempt=0; \
	max_restarts=5; \
	while [ $$attempt -lt $$max_restarts ]; do \
		attempt=$$((attempt + 1)); \
		echo "[supervisor] Attempt $$attempt/$$max_restarts"; \
		cargo run --release 2>&1; \
		exit_code=$$?; \
		if [ $$exit_code -eq 0 ]; then \
			echo "[supervisor] Clean exit (code 0) — not restarting"; \
			break; \
		fi; \
		if [ $$attempt -lt $$max_restarts ]; then \
			backoff=$$((2 ** attempt)); \
			echo "[supervisor] Crashed (exit code $$exit_code) — restarting in $${backoff}s..."; \
			sleep $$backoff; \
		else \
			echo "[supervisor] Max restarts reached — giving up"; \
		fi; \
	done

build: ## Build release binary
	@echo "🔨 Building release..."
	cargo build --release
	@echo "  Binary: target/release/$(APP_NAME)"
	@ls -lh target/release/$(APP_NAME) 2>/dev/null | awk '{print "  Size:", $$5}'

# =============================================================================
# QUALITY GATES
# =============================================================================

test: ## Run all tests
	@echo "🧪 Running tests..."
	cargo test
	@echo ""
	@echo "  ✅ All tests passed"

scoped-check: ## Run tests ONLY for crates touched by current diff (see .claude/rules/project/testing-scope.md)
	@bash .claude/hooks/scoped-test-runner.sh

full-qa: ## Force full workspace tests (overrides scoped-check)
	@FULL_QA=1 bash .claude/hooks/scoped-test-runner.sh

check: fmt clippy test ## Full quality check (fmt + clippy + test)
	@echo ""
	@echo "  ✅ All quality gates passed"

fmt: ## Format code
	@cargo fmt
	@echo "  ✅ Formatted"

clippy: ## Lint with clippy (zero warnings)
	@cargo clippy --workspace --all-targets -- -D warnings -W clippy::perf
	@echo "  ✅ Clippy clean"

clean: ## Clean build artifacts
	@cargo clean
	@echo "  🧹 Cleaned"

# =============================================================================
# DOCKER INFRASTRUCTURE
# =============================================================================

docker-up: ## Start all Docker infrastructure (8 services) + auto-init QuestDB schema
	@echo "🐳 Starting Docker infrastructure..."
	docker compose -f $(COMPOSE_FILE) up -d
	@echo ""
	@$(MAKE) docker-status
	@echo "🗄️  Initializing QuestDB schema (15 tables + 18 views)..."
	@bash scripts/questdb-init.sh

docker-down: ## Stop all Docker infrastructure
	@echo "🐳 Stopping Docker infrastructure..."
	docker compose -f $(COMPOSE_FILE) down

docker-restart: ## Restart all Docker infrastructure + re-init QuestDB schema
	@echo "🐳 Restarting Docker infrastructure..."
	docker compose -f $(COMPOSE_FILE) down
	docker compose -f $(COMPOSE_FILE) up -d
	@echo ""
	@$(MAKE) docker-status
	@echo "🗄️  Initializing QuestDB schema (15 tables + 18 views)..."
	@bash scripts/questdb-init.sh

docker-status: ## Show Docker container health
	@echo ""
	@echo "  Docker Container Status"
	@echo "  ───────────────────────"
	@docker ps --format "  {{.Names}}\t{{.Status}}" --filter "name=tv-" | sort
	@echo ""

docker-logs: ## Tail Docker logs (all services)
	docker compose -f $(COMPOSE_FILE) logs -f --tail 50

questdb-init: ## Create all QuestDB tables + materialized views (idempotent)
	@bash scripts/questdb-init.sh

# =============================================================================
# HEALTH & STATUS
# =============================================================================

health: ## Check app health endpoint
	@echo "❤️  Health check:"
	@curl -s http://localhost:$(APP_PORT)/health 2>/dev/null | python3 -m json.tool 2>/dev/null || echo "  ⚠️  App not running (http://localhost:$(APP_PORT)/health)"

status: ## Full system status (Docker + App + QuestDB + Valkey)
	@echo ""
	@echo "  ╔════════════════════════════════════════╗"
	@echo "  ║   tickvault — System Status     ║"
	@echo "  ╚════════════════════════════════════════╝"
	@echo ""
	@echo "  📦 Docker Services:"
	@docker ps --format "     {{.Names}}\t{{.Status}}" --filter "name=tv-" 2>/dev/null | sort || echo "     ⚠️  Docker not running"
	@echo ""
	@echo "  🚀 Application:"
	@if curl -s http://localhost:$(APP_PORT)/health > /dev/null 2>&1; then \
		echo "     ✅ Running on port $(APP_PORT)"; \
		echo "     $$(curl -s http://localhost:$(APP_PORT)/health)"; \
	else \
		echo "     ⚠️  Not running"; \
	fi
	@echo ""
	@echo "  📊 QuestDB:"
	@curl -sf --max-time 2 http://localhost:9000/exec?query=SHOW+TABLES > /dev/null 2>&1 \
		&& echo "     ✅ Console: http://localhost:9000" \
		|| echo "     ⚠️  Not reachable on port 9000"
	@echo ""
	@echo "  💾 Valkey:"
	@docker exec tv-valkey valkey-cli ping 2>/dev/null | grep -q PONG \
		&& echo "     ✅ PONG — connected" \
		|| echo "     ⚠️  Not reachable"
	@echo ""
	@echo "  📈 Monitoring URLs:"
	@echo "     Grafana:    http://localhost:3000"
	@echo "     Prometheus: http://localhost:9090"
	@echo "     Jaeger:     http://localhost:16686"
	@echo "     QuestDB:    http://localhost:9000"
	@echo "     App:        http://localhost:$(APP_PORT)"
	@echo ""

open: ## Open DLT Control Panel in browser
	@open http://localhost:$(APP_PORT)/portal

# =============================================================================
# MONITORING — Open dashboards in browser
# =============================================================================

grafana: ## Open Grafana dashboard (localhost:3000)
	@open http://localhost:3000

questdb: ## Open QuestDB console (localhost:9000)
	@open http://localhost:9000

jaeger: ## Open Jaeger UI (localhost:16686)
	@open http://localhost:16686

prometheus: ## Open Prometheus UI (localhost:9090)
	@open http://localhost:9090

# =============================================================================
# QUALITY GATES — Extended tools
# =============================================================================

audit: ## Security audit (cargo audit + cargo deny)
	@echo "🔒 Running security audit..."
	cargo audit
	cargo deny check
	@echo "  ✅ Security audit clean"

coverage: ## Code coverage report (HTML, 99% threshold)
	@echo "📊 Running coverage..."
	cargo llvm-cov --workspace --fail-under-lines 99
	cargo llvm-cov --workspace --html --output-dir target/llvm-cov
	@echo "  📊 Report: target/llvm-cov/html/index.html"

bench: ## Run benchmarks (Criterion)
	@echo "⚡ Running benchmarks..."
	cargo bench --workspace

geiger: ## Unsafe code audit (cargo geiger)
	@echo "☢️  Scanning for unsafe code..."
	cargo geiger --all-features --all-targets

typos: ## Spell check code and docs
	@echo "📝 Checking for typos..."
	typos .
	@echo "  ✅ No typos found"

quality: ## Full quality gates (all 6 CI stages)
	@./scripts/quality-full.sh

doc: ## Build documentation
	@echo "📖 Building docs..."
	cargo doc --workspace --no-deps
	@echo "  📖 Open: target/doc/index.html"

bootstrap: ## First-time setup (run once after cloning)
	@./scripts/bootstrap.sh

# =============================================================================
# OBSERVABILITY STACK — Full automation
# =============================================================================

obs: ## Full auto-setup observability stack (zero-touch: pull, start, verify, open)
	@./scripts/setup-observability.sh

obs-verify: ## Verify observability stack health (no restart)
	@./scripts/setup-observability.sh --verify

obs-restart: ## Tear down + fresh restart of observability stack
	@./scripts/setup-observability.sh --restart

obs-open: ## Open all monitoring dashboards in browser
	@./scripts/setup-observability.sh --verify --no-open
	@echo "  Opening dashboards..."
	@open http://localhost:3000/d/tv-system-overview 2>/dev/null || xdg-open http://localhost:3000/d/tv-system-overview 2>/dev/null || echo "  Open: http://localhost:3000/d/tv-system-overview"
	@open http://localhost:9090/targets 2>/dev/null || xdg-open http://localhost:9090/targets 2>/dev/null || true
	@open http://localhost:16686 2>/dev/null || xdg-open http://localhost:16686 2>/dev/null || true
	@open http://localhost:8080 2>/dev/null || xdg-open http://localhost:8080 2>/dev/null || true
	@open http://localhost:9000 2>/dev/null || xdg-open http://localhost:9000 2>/dev/null || true

# =============================================================================
# MONITORING — Open individual dashboards in browser
# =============================================================================

traefik: ## Open Traefik dashboard (localhost:8080)
	@open http://localhost:8080 2>/dev/null || xdg-open http://localhost:8080 2>/dev/null || echo "  Open: http://localhost:8080"

alloy: ## Open Alloy UI (localhost:12345)
	@open http://localhost:12345 2>/dev/null || xdg-open http://localhost:12345 2>/dev/null || echo "  Open: http://localhost:12345"

loki: ## Open Loki status (localhost:3100/ready)
	@open http://localhost:3100/ready 2>/dev/null || xdg-open http://localhost:3100/ready 2>/dev/null || echo "  Open: http://localhost:3100/ready"



# =============================================================================
# AWS DEPLOYMENT — Phase 8 one-time bootstrap helpers
# =============================================================================

aws-bootstrap-check: ## Verify prerequisites for the first terraform apply
	@echo "Checking AWS deployment prerequisites..."
	@command -v terraform >/dev/null 2>&1 || { echo "  FAIL: terraform not installed"; exit 1; }
	@command -v aws >/dev/null 2>&1 || { echo "  FAIL: aws CLI not installed"; exit 1; }
	@aws sts get-caller-identity >/dev/null 2>&1 || { echo "  FAIL: aws CLI not configured (run 'aws configure')"; exit 1; }
	@REGION=$$(aws configure get region); [ "$$REGION" = "ap-south-1" ] || { echo "  FAIL: default region must be ap-south-1, got $$REGION"; exit 1; }
	@echo "  PASS: terraform + aws CLI + ap-south-1 region"
	@echo ""
	@echo "Next: run 'make aws-init' to initialize Terraform"

aws-ami: ## Look up the latest Ubuntu 24.04 LTS AMI in ap-south-1 (export TF_VAR_ami_id)
	@AMI=$$(aws ec2 describe-images \
		--region ap-south-1 \
		--owners 099720109477 \
		--filters 'Name=name,Values=ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*' \
		--query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text); \
	echo "Latest Ubuntu 24.04 AMI in ap-south-1: $$AMI"; \
	echo "export TF_VAR_ami_id=$$AMI"

aws-operator-cidr: ## Print your current public IP CIDR for TF_VAR_operator_cidr
	@IP=$$(curl -s ifconfig.me); echo "Your public IP: $$IP"; echo "export TF_VAR_operator_cidr=$$IP/32"

aws-keypair: ## Create the tv-prod-key SSH key pair (stores private key in ~/.ssh)
	@if [ -f ~/.ssh/tv-prod-key.pem ]; then \
		echo "  SKIP: ~/.ssh/tv-prod-key.pem already exists"; \
	else \
		aws ec2 create-key-pair \
			--key-name tv-prod-key \
			--region ap-south-1 \
			--query KeyMaterial --output text > ~/.ssh/tv-prod-key.pem && \
		chmod 400 ~/.ssh/tv-prod-key.pem && \
		echo "  Created ~/.ssh/tv-prod-key.pem"; \
	fi

aws-init: aws-bootstrap-check ## Initialize Terraform in deploy/aws/terraform/
	@cd deploy/aws/terraform && terraform init

aws-plan: aws-bootstrap-check ## Show Terraform plan for the DLT AWS stack
	@cd deploy/aws/terraform && terraform plan

aws-apply: aws-bootstrap-check ## Apply the Terraform stack (requires TF_VAR_ami_id + TF_VAR_operator_cidr)
	@if [ -z "$$TF_VAR_ami_id" ]; then \
		echo "FAIL: TF_VAR_ami_id is not set. Run 'make aws-ami' first."; exit 1; \
	fi
	@if [ -z "$$TF_VAR_operator_cidr" ]; then \
		echo "FAIL: TF_VAR_operator_cidr is not set. Run 'make aws-operator-cidr' first."; exit 1; \
	fi
	@cd deploy/aws/terraform && terraform apply

aws-outputs: ## Show Terraform outputs (instance_id, elastic_ip, etc.)
	@cd deploy/aws/terraform && terraform output

aws-ssm: ## Open a Session Manager shell on the DLT instance (no SSH)
	@cd deploy/aws/terraform && INSTANCE=$$(terraform output -raw instance_id) && \
	aws ssm start-session --region ap-south-1 --target $$INSTANCE

aws-ssm-command: ## Run 'journalctl -u tickvault -n 50' on the instance via SSM
	@cd deploy/aws/terraform && INSTANCE=$$(terraform output -raw instance_id) && \
	CMD_ID=$$(aws ssm send-command --region ap-south-1 --instance-ids $$INSTANCE \
		--document-name AWS-RunShellScript \
		--parameters 'commands=["journalctl -u tickvault -n 50 --no-pager"]' \
		--query Command.CommandId --output text) && \
	sleep 3 && \
	aws ssm get-command-invocation --region ap-south-1 \
		--command-id $$CMD_ID --instance-id $$INSTANCE \
		--query StandardOutputContent --output text

aws-cost: ## Show the current month AWS bill estimate (via Cost Explorer)
	@aws ce get-cost-and-usage \
		--region ap-south-1 \
		--time-period Start=$$(date -u +%Y-%m-01),End=$$(date -u +%Y-%m-%d) \
		--granularity MONTHLY --metrics UnblendedCost --output text 2>/dev/null || \
	echo "  (Cost Explorer requires explicit enablement + 24h delay)"

# -----------------------------------------------------------------------------
# Phase 2.2 / 5.2 — Zero-touch observability operator commands
# -----------------------------------------------------------------------------

tail-errors: ## Live tail of data/logs/errors.jsonl.* with jq pretty-print
	@echo "Tailing ERROR-only JSONL stream (data/logs/errors.jsonl.*). Ctrl-C to stop."
	@if ls data/logs/errors.jsonl* >/dev/null 2>&1; then \
		tail -n 50 -F data/logs/errors.jsonl* 2>/dev/null | \
			(command -v jq >/dev/null && jq -c '{ts: .timestamp, code, severity, target, message}' || cat); \
	else \
		echo "  (no errors.jsonl* files yet — app may not have run since Phase 2 shipped)"; \
		exit 0; \
	fi

errors-summary: ## Print data/logs/errors.summary.md (refreshed every 60s by app)
	@if [ -f data/logs/errors.summary.md ]; then \
		cat data/logs/errors.summary.md; \
	else \
		echo "  (no errors.summary.md yet — app may not have run since Phase 5 shipped)"; \
	fi

triage-dry-run: ## Run the error-triage hook without executing auto-fixes
	@bash .claude/hooks/error-triage.sh

triage-execute: ## Run the error-triage hook WITH auto-fix execution
	@bash .claude/hooks/error-triage.sh --execute-autofix

validate-automation: ## End-to-end validation of the zero-touch chain (20 checks)
	@bash scripts/validate-automation.sh

doctor: ## Total-system health check (one command — every section explicit pass/fail)
	@bash scripts/doctor.sh

mcp-doctor: ## Probe every endpoint the tickvault-logs MCP server reads (4 checks)
	@bash scripts/mcp-doctor.sh

100pct-audit: ## Real-time 100% Audit Tracker — every dimension w/ proof (M5)
	@bash scripts/100pct-audit.sh

100pct-audit-ci: ## Same as 100pct-audit but exits non-zero on any GAP (CI mode)
	@bash scripts/100pct-audit.sh --ci

100pct-audit-json: ## Machine-readable JSON output for dashboards
	@bash scripts/100pct-audit.sh --json

# =============================================================================
# Claude Co-work Dispatch — uniform entry point from every terminal
# =============================================================================
# Fires a fresh Claude Co-work session in the GitHub cloud runner to execute
# an arbitrary ops instruction. Works identically from:
#   - Claude Code CLI sessions on your Mac
#   - Claude Co-work web sessions on laptop
#   - Any terminal with `gh` CLI installed
#   - Mobile: use the GitHub mobile app's "Run workflow" UI directly
#
# Requires: gh CLI authenticated against SJParthi/tickvault.
# =============================================================================

dispatch: ## Dispatch a mobile-style command to Claude Co-work. Usage: make dispatch MSG="check prom"
	@if [ -z "$(MSG)" ]; then \
		echo "  Usage: make dispatch MSG=\"your instruction\""; \
		echo ""; \
		echo "  Examples:"; \
		echo "    make dispatch MSG=\"check prometheus for ticks_dropped\""; \
		echo "    make dispatch MSG=\"run make doctor and summarise\""; \
		echo "    make dispatch MSG=\"review the latest open PR\""; \
		echo "    make dispatch MSG=\"restart depth if any stale-spot alert is firing\""; \
		exit 1; \
	fi
	@command -v gh >/dev/null 2>&1 || { echo "  gh CLI not installed. brew install gh"; exit 1; }
	@echo "  Dispatching to Claude Co-work: $(MSG)"
	@gh workflow run claude-mobile-command.yml \
		-f instruction="$(MSG)" \
		-f allow_write=true
	@echo "  Queued. Follow progress with: make dispatch-status"

dispatch-readonly: ## Dispatch in read-only mode (no branches/PRs/commits). Usage: make dispatch-readonly MSG="..."
	@if [ -z "$(MSG)" ]; then \
		echo "  Usage: make dispatch-readonly MSG=\"your instruction\""; \
		exit 1; \
	fi
	@command -v gh >/dev/null 2>&1 || { echo "  gh CLI not installed"; exit 1; }
	@echo "  Dispatching (read-only) to Claude Co-work: $(MSG)"
	@gh workflow run claude-mobile-command.yml \
		-f instruction="$(MSG)" \
		-f allow_write=false

dispatch-status: ## Show the last 5 Claude Co-work dispatch runs
	@command -v gh >/dev/null 2>&1 || { echo "  gh CLI not installed"; exit 1; }
	@gh run list --workflow=claude-mobile-command.yml --limit 5

dispatch-logs: ## Tail the logs of the most recent Claude Co-work dispatch run
	@command -v gh >/dev/null 2>&1 || { echo "  gh CLI not installed"; exit 1; }
	@RUN_ID=$$(gh run list --workflow=claude-mobile-command.yml --limit 1 --json databaseId --jq '.[0].databaseId'); \
	if [ -z "$$RUN_ID" ]; then echo "  No dispatch runs yet"; exit 0; fi; \
	gh run view "$$RUN_ID" --log

dispatch-check: ## Common preset — ask Claude to run the full health check
	@$(MAKE) dispatch MSG="run /health in repo context and post the 14-row table as the step summary"

dispatch-audit: ## Common preset — ask Claude to run the 100% audit
	@$(MAKE) dispatch MSG="run scripts/100pct-audit.sh and post PASS/GAP counts + any GAPs as the step summary"
