.PHONY: help restart-firecracker-lima clean-firecracker-microvms

help:
	@echo "restart-firecracker-lima    Restart the outer Lima VM and retain Exo state"
	@echo "clean-firecracker-microvms  Restart it and remove per-microVM state"

restart-firecracker-lima:
	./support/firecracker/lima-admin.sh restart

clean-firecracker-microvms:
	./support/firecracker/lima-admin.sh clean
