# Config
REPO_URL = https://github.com/logos-messaging/logos-messaging-nim
REPO_DIR = logos-messaging-nim
OUTPUT_DIR = libs
LIBWAKU_NIM_PARAMS ?= --undef:metrics

# Platform-specific library name
ifeq ($(shell uname),Darwin)
    LIB_NAME = libwaku.dylib
    # macOS needs SDKROOT for Clang to find <string.h> and friends.
    export SDKROOT ?= $(shell xcrun --show-sdk-path)
else
    LIB_NAME = libwaku.so
endif

.PHONY: all clean setup build copy

all: setup build copy

# 1. Setup: Clone and initialize submodules
setup:
	@echo "--- [1/3] Checking dependencies ---"
	@if ! command -v nim > /dev/null; then \
		echo "Error: Nim is not installed. Please run: brew install nim"; \
		exit 1; \
	fi
	@echo "--- Checking repository ---"
	@if [ ! -d "$(REPO_DIR)" ]; then \
		echo "Cloning logos-messaging-nim..."; \
		git clone $(REPO_URL) $(REPO_DIR); \
	else \
		echo "Repository exists. Updating..."; \
		cd $(REPO_DIR) && git pull; \
	fi
	@echo "--- Initializing Submodules ---"
	cd $(REPO_DIR) && git submodule update --init --recursive

# 2. Build: Use the repo's internal 'make'
build:
	@echo "--- [2/3] Building libwaku ---"
	@echo "Using SDKROOT: $(SDKROOT)"
	@# Update vendored deps
	cd $(REPO_DIR) && $(MAKE) update
	@# Compile
	cd $(REPO_DIR) && $(MAKE) libwaku BUILD_COMMAND="libwakuDynamic $(LIBWAKU_NIM_PARAMS)"

# 3. Retrieve: Copy the result
copy:
	@echo "--- [3/3] Retrieving library ---"
	@mkdir -p $(OUTPUT_DIR)
	@if [ -f "$(REPO_DIR)/build/$(LIB_NAME)" ]; then \
		cp "$(REPO_DIR)/build/$(LIB_NAME)" "$(OUTPUT_DIR)/$(LIB_NAME)"; \
	else \
		echo "Error: Could not find $(LIB_NAME) in $(REPO_DIR)/build/"; \
		exit 1; \
	fi
	@echo "Success! Library located at: ./$(OUTPUT_DIR)/$(LIB_NAME)"

clean:
	rm -rf $(OUTPUT_DIR)
	rm -rf $(REPO_DIR)
