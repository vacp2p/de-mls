## ref: https://gist.github.com/enil/e4af160c745057809053329df4ba1dc2

GIT=git
GIT_SUBMODULES=$(shell sed -nE 's/path = +(.+)/\1\/.git/ p' .gitmodules | paste -s -)

.PHONY: deps
deps: $(GIT_SUBMODULES)

$(GIT_SUBMODULES): %/.git: .gitmodules
	$(GIT) submodule init
	$(GIT) submodule update $*
	@touch $@

.EXPORT_ALL_VARIABLES:

REDIS_PORT=6379
ANVIL_PORT=8545

start:
	docker compose up -d
	until cast chain-id --rpc-url "http://localhost:${ANVIL_PORT}" 2> /dev/null; do sleep 1; done
	cd contracts && forge script --broadcast --rpc-url "http://localhost:${ANVIL_PORT}" script/Deploy.s.sol:Deploy --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80

stop:
	docker compose down
