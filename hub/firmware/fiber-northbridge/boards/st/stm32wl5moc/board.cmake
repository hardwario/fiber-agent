# SPDX-License-Identifier: Apache-2.0

board_runner_args(jlink "--device=STM32WL55JC" "--speed=4000")

include(${ZEPHYR_BASE}/boards/common/jlink.board.cmake)

# The app (app/CMakeLists.txt) sets NB_JLINK_HEX: nbboot + app + app_valid record in one hex, so
# a J-Link flash leaves a layout the boot stub starts (plan/control/radio/northbridge_update.md).
if(DEFINED NB_JLINK_HEX)
  set_property(TARGET runners_yaml_props_target PROPERTY hex_file ${NB_JLINK_HEX})
endif()
