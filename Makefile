.PHONY: lint typecheck install install-dev docs docs-serve clean fix

# Default target
all: lint typecheck

# Linting (for CI)
lint:
	hatch run dev:lint

# Auto-fix everything possible locally
fix:
	hatch run dev:format
	hatch run dev:ruff check . --fix

# Type checking
typecheck:
	hatch run dev:typecheck

# Install dependencies (default environment)
install:
	pip install hatch
	hatch env create default

# Install dev dependencies (including default dependencies)
install-dev:
	pip install hatch
	hatch env create dev

# Install local dependencies in dev mode
ROOT_DIR := $(shell realpath .)
install-local:
	# Clean up any existing installations first
	pip uninstall -y murmur-slim murmur_slim murmur-langgraph murmur_langgraph murmur-swarm murmur_swarm || true
	
	# First build all packages
	cd $(ROOT_DIR)/lib/murmur && hatch build
	cd $(ROOT_DIR)/lib/murmur/murmur/clients/langgraph && hatch build
	cd $(ROOT_DIR)/lib/murmur/murmur/clients/swarm && hatch build
	
	# Install murmur-slim first (the base package)
	hatch run dev:pip install --no-deps --force-reinstall --ignore-requires-python $(ROOT_DIR)/lib/murmur/dist/*.whl || exit 1
	
	# Verify murmur-slim installation
	pip show murmur_slim || exit 1
	
	# Then install the client packages that depend on it
	hatch run dev:pip install --no-deps --force-reinstall --ignore-requires-python $(ROOT_DIR)/lib/murmur/murmur/clients/langgraph/dist/*.whl
	hatch run dev:pip install --no-deps --force-reinstall --ignore-requires-python $(ROOT_DIR)/lib/murmur/murmur/clients/swarm/dist/*.whl


# Build documentation
docs:
	mkdocs build -f docs/mkdocs.yaml

# Serve documentation
docs-serve:
	mkdocs serve -f docs/mkdocs.yaml

# Clean up
clean:
	find . -type d -name dist -exec rm -rf {} +
	rm -rf build/
	rm -rf *.egg-info/
	find . -type d -name __pycache__ -exec rm -rf {} +
	find . -type f -name "*.pyc" -delete 
