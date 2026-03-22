# =============================================================================
# dhan-live-trader — Development & Operations Makefile
# =============================================================================
# Usage: make <target>
# Run `make help` to see all available targets.
# =============================================================================

.PHONY: help run run-supervised stop build test check fmt clippy clean \
        docker-up docker-down docker-restart docker-status docker-logs \
        health status open grafana questdb jaeger prometheus traefik alloy loki \
        obs obs-verify obs-restart obs-open \
        logs app-pid \
        audit coverage bench geiger typos quality doc bootstrap \

# ---- Configuration ----
APP_NAME       := dhan-live-trader
APP_PORT       := 3001
DOCKER_DIR     := deploy/docker
COMPOSE_FILE   := $(DOCKER_DIR)/docker-compose.yml

# ---- Help ----
help: ## Show this help
	@echo ""
	@echo "  dhan-live-trader — Development & Operations"
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
	@echo "Starting dhan-live-trader with crash supervision (max 5 restarts)..."
	@attempt=0; \
	max_restarts=5; \
	while [ $$attempt -lt $$max_restarts ]; do \
		attempt=$$((attempt + 1)); \
		echo "[supervisor] Attempt $$attempt/$$max_restarts"; \
		cargo run --release 2>&1; \
		exit_code=$$?; \
		if [ $$exit_code -eq 0 ]; then \
			echo "[supervisor] Clean exit — not restarting"; \
			break; \
		fi; \
		if [ $$attempt -lt $$max_restarts ]; then \
			backoff=$$((2 ** attempt)); \
			echo "[supervisor] Crashed (exit $$exit_code) — restarting in $${backoff}s..."; \
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

docker-up: ## Start all Docker infrastructure (8 services)
	@echo "🐳 Starting Docker infrastructure..."
	docker compose -f $(COMPOSE_FILE) up -d
	@echo ""
	@$(MAKE) docker-status

docker-down: ## Stop all Docker infrastructure
	@echo "🐳 Stopping Docker infrastructure..."
	docker compose -f $(COMPOSE_FILE) down

docker-restart: ## Restart all Docker infrastructure
	@echo "🐳 Restarting Docker infrastructure..."
	docker compose -f $(COMPOSE_FILE) down
	docker compose -f $(COMPOSE_FILE) up -d
	@echo ""
	@$(MAKE) docker-status

docker-status: ## Show Docker container health
	@echo ""
	@echo "  Docker Container Status"
	@echo "  ───────────────────────"
	@docker ps --format "  {{.Names}}\t{{.Status}}" --filter "name=dlt-" | sort
	@echo ""

docker-logs: ## Tail Docker logs (all services)
	docker compose -f $(COMPOSE_FILE) logs -f --tail 50

# =============================================================================
# HEALTH & STATUS
# =============================================================================

health: ## Check app health endpoint
	@echo "❤️  Health check:"
	@curl -s http://localhost:$(APP_PORT)/health 2>/dev/null | python3 -m json.tool 2>/dev/null || echo "  ⚠️  App not running (http://localhost:$(APP_PORT)/health)"

status: ## Full system status (Docker + App + QuestDB + Valkey)
	@echo ""
	@echo "  ╔════════════════════════════════════════╗"
	@echo "  ║   dhan-live-trader — System Status     ║"
	@echo "  ╚════════════════════════════════════════╝"
	@echo ""
	@echo "  📦 Docker Services:"
	@docker ps --format "     {{.Names}}\t{{.Status}}" --filter "name=dlt-" 2>/dev/null | sort || echo "     ⚠️  Docker not running"
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
	@docker exec dlt-valkey valkey-cli ping 2>/dev/null | grep -q PONG \
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
	@open http://localhost:3000/d/dlt-system-overview 2>/dev/null || xdg-open http://localhost:3000/d/dlt-system-overview 2>/dev/null || echo "  Open: http://localhost:3000/d/dlt-system-overview"
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

