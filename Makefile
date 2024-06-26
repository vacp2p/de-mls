## ref: https://gist.github.com/enil/e4af160c745057809053329df4ba1dc2

GIT=git
GIT_SUBMODULES=$(shell sed -nE 's/path = +(.+)/\1\/.git/ p' .gitmodules | paste -s -)

.PHONY: deps
deps: $(GIT_SUBMODULES)

$(GIT_SUBMODULES): %/.git: .gitmodules
	$(GIT) submodule init
	$(GIT) submodule update $*
	@touch $@
