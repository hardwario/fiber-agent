load test_helper

@test "fiber.dtso compiles and carries the Hub's nodes" {
  run make -C "$REPO_ROOT" dtbo
  [ "$status" -eq 0 ]
  run fdtdump "$REPO_ROOT/build/fiber.dtbo"
  [[ "$output" == *'compatible = "nxp,pcf85063a"'* ]]
  [[ "$output" == *'compatible = "maxim,ds2482"'* ]]
  [[ "$output" == *'compatible = "spidev"'* ]]
  [[ "$output" == *'__fixups__'* ]]
}

@test "fiber.modules lists the five modules" {
  [ "$(tr '\n' ' ' < "$REPO_ROOT/boot/fiber.modules")" = "i2c-dev spi-bcm2835 spidev ds2482 cdc_acm " ]
}
