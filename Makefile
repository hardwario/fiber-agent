# fiber-agent developer targets. See docs/superpowers/plans/ for what each builds.
UPSTREAM_TAG ?= v1.1.0
HUB ?=

.PHONY: check dtbo stage deploy

check:
	shellcheck bootstrap.sh files/fiber-hosts-seed.sh $(wildcard bin/*) $(wildcard lib/fiber/*.sh) $(wildcard tools/*.sh)
	bats tests

dtbo: build/fiber.dtbo

build/fiber.dtbo: boot/fiber.dtso
	mkdir -p build
	dtc -@ -I dts -O dtb -o $@ $<

stage: dtbo
	tools/stage.sh $(UPSTREAM_TAG) build/root

deploy:
	@test -n "$(HUB)" || { echo 'usage: make HUB=user@host deploy'; exit 2; }
	tools/deploy-bench.sh $(HUB)
