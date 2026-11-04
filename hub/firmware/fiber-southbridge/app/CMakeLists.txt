# SPDX-License-Identifier: Apache-2.0
cmake_minimum_required(VERSION 3.20.0)

list(APPEND BOARD_ROOT ${CMAKE_CURRENT_LIST_DIR}/..)

find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(fiber_southbridge_app)

# VERSION: south-zephyr <SB_FW_VERSION> g<SB_GIT_SHA>. The Hub compares it with the manifest
# of the image it carries and re-flashes the SB on a mismatch
# (proximos-v2 plan/system/southbridge_update.md §3.1).
set(SB_FW_VERSION "1.1.0")

# The last commit that touched what goes into the image (app/, boards/, west.yml), taken when
# CMake configures (a pristine build re-reads it). Path-scoped, not HEAD, so a Hub-only commit
# of the monorepo, or one to the SB docs, tests or tools, does not change the identity and
# re-flashes no SB. A shallow clone may answer with its boundary commit: the CI fetches full
# history for it (GIT_DEPTH 0). 00000000 when this is not a git checkout.
execute_process(
  COMMAND git log -1 --format=%H -- app boards west.yml
  WORKING_DIRECTORY ${CMAKE_CURRENT_LIST_DIR}/..
  OUTPUT_VARIABLE SB_GIT_SHA
  OUTPUT_STRIP_TRAILING_WHITESPACE
  ERROR_QUIET
  RESULT_VARIABLE sb_git_rc)
if(NOT sb_git_rc EQUAL 0 OR NOT SB_GIT_SHA MATCHES "^[0-9a-f]+$")
  set(SB_GIT_SHA "00000000")
endif()
string(SUBSTRING "${SB_GIT_SHA}00000000" 0 8 SB_GIT_SHA)
target_compile_definitions(app PRIVATE
  SB_FW_VERSION_STR="${SB_FW_VERSION}"
  SB_GIT_SHA_STR="${SB_GIT_SHA}"
)
# Read by tools/sb_package.py (through system/scripts/sb-package.sh) to write the manifest of
# exactly this build.
file(WRITE ${CMAKE_BINARY_DIR}/zephyr/sb_build.json
  "{\"fw_version\": \"${SB_FW_VERSION}\", \"git_sha\": \"${SB_GIT_SHA}\"}\n")

target_sources(app PRIVATE
	src/main.c
	src/gpio_table.c
	src/uart_proto.c
	src/cmd_dispatch.c
	src/adc_read.c
	src/led_pwm.c
	src/shell_cmds.c
)
