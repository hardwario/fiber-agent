cmake_minimum_required(VERSION 3.20.0)
list(APPEND BOARD_ROOT ${CMAKE_CURRENT_LIST_DIR}/..)

# Bench-only: bridges the P2P HDLC protocol over the RTT *shell* channel (0),
# carrying frames as `p2p_inject <hex>` in / `P2P_OUT <hex>` out text lines
# instead of over USART1, for rigs with no CM4/USB-serial wired to the real
# UART (J-Link RTT only). Never for production - USART1 is the real link there.
# Build with `west build -b stm32wl5moc app -- -DBENCH_RTT_BRIDGE=ON`.
option(BENCH_RTT_BRIDGE "Bridge the P2P protocol over the RTT shell channel instead of USART1 (bench only)" OFF)
if(BENCH_RTT_BRIDGE)
  # A full-frame p2p_inject must land in ONE RTT write, or the JoinAccept
  # TX_SCHEDULE misses the node's ~1 s RX1 window; the default 16 B channel-0
  # down-buffer chunks it too slowly. Pulled in here so -DBENCH_RTT_BRIDGE=ON
  # alone is sufficient - nothing extra to remember on the build line.
  list(APPEND EXTRA_CONF_FILE ${CMAKE_CURRENT_LIST_DIR}/bench_rtt_bridge.conf)
endif()

# The image is the TOWER gateway (proximos-v2 plan/control/radio/p2p_tower_gateway.md): the
# Northbridge ACKs locally and holds the peer table and the downlink queue the host feeds it
# over USART1. There is no other link protocol to choose; the proto-1 modem image is the
# rollback.
#
# Bench-only TOWER-over-LoRa P0 gateway: the Northbridge runs the TOWER radio protocol itself
# (auto-ACK, one queued downlink) driven from the RTT shell (`twr ...`, TWR_* event lines),
# with the static bench key. No host link, no cmd_dispatch. Needs the bench RTT buffers, so it
# is only accepted together with BENCH_RTT_BRIDGE:
# `west build -b stm32wl5moc app -- -DBENCH_RTT_BRIDGE=ON -DTOWER_P0_GATEWAY=ON`.
option(TOWER_P0_GATEWAY "TOWER P0 bench gateway over the RTT shell (bench only)" OFF)
set(TOWER_P0_NODE_ADDR "0x00000413" CACHE STRING
  "TOWER P0 gateway: boot default node address (low 32 bits of the node DevEUI)")
if(TOWER_P0_GATEWAY)
  if(NOT BENCH_RTT_BRIDGE)
    message(FATAL_ERROR "TOWER_P0_GATEWAY is a bench image: add -DBENCH_RTT_BRIDGE=ON")
  endif()
  list(APPEND EXTRA_CONF_FILE ${CMAKE_CURRENT_LIST_DIR}/tower_p0_gateway.conf)
endif()

# What `west flash` programs: nbboot + app + app_valid record, made after the link (below). The
# board's board.cmake hands it to the runners, which read it while find_package() runs.
set(NB_JLINK_HEX ${CMAKE_BINARY_DIR}/zephyr/northbridge_jlink.hex)

find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(p2p_northbridge_app)

# EVT_BOOT / GET_INFO git_sha: the first 4 bytes of the last commit that touched what goes
# into the app image (app/, boards/, patches/, west.yml), taken when CMake configures (a
# pristine build re-reads it). Path-scoped, not HEAD, so a Hub-only commit of the monorepo, or
# one to the NB docs, tests or tools, leaves the image and its crc32 alone
# (northbridge_update.md §3.2). A shallow clone may answer with its boundary commit: the CI
# fetches full history for it (GIT_DEPTH 0). 0 when this is not a git checkout.
execute_process(
  COMMAND git log -1 --format=%H -- app boards patches west.yml
  WORKING_DIRECTORY ${CMAKE_CURRENT_LIST_DIR}/..
  OUTPUT_VARIABLE NB_GIT_SHA
  OUTPUT_STRIP_TRAILING_WHITESPACE
  ERROR_QUIET
  RESULT_VARIABLE nb_git_rc)
if(NOT nb_git_rc EQUAL 0 OR NOT NB_GIT_SHA MATCHES "^[0-9a-f]+$")
  set(NB_GIT_SHA "00000000")
endif()
string(SUBSTRING "${NB_GIT_SHA}00000000" 0 8 NB_GIT_SHA)
target_compile_definitions(app PRIVATE APP_GIT_SHA=0x${NB_GIT_SHA}u)

if(BENCH_RTT_BRIDGE)
  target_compile_definitions(app PRIVATE APP_P2P_BENCH_RTT_BRIDGE=1)
endif()

if(TOWER_P0_GATEWAY)
  target_compile_definitions(app PRIVATE
    APP_TWR_P0_GATEWAY=1
    TWR_NODE_ADDR_DEFAULT=${TOWER_P0_NODE_ADDR}u
  )
  target_sources(app PRIVATE
    src/twr_gw.c
    src/twr_radio.c
  )
else()
  target_sources(app PRIVATE
    src/cmd_dispatch.c
    src/twr_gateway.c
    src/twr_peers.c
    src/twr_dlq.c
    src/twr_gwc.c
    src/twr_ackctrl.c
    src/twr_evq.c
    src/twr_link.c
    src/twr_cmd.c
    # Update through the Hub: ENTER_BOOTLOADER, GET_INFO app_image_len / app_crc32.
    src/nb_boot.c
    ${CMAKE_CURRENT_LIST_DIR}/../nbboot/src/nb_app_valid.c
  )
endif()

target_sources(app PRIVATE
  src/main.c
  src/hdlc.c
  src/uart_link.c
  src/diag.c
  src/twr_crypto.c
  src/twr_frame.c
  src/twr_airtime.c
)

# The patched LoRa driver (patches/README.md) is compiled from this repo, in place of the
# workspace's vendored zephyr/drivers/lora/loramac-node/sx126x.c: nothing to copy into the
# zephyr tree, and a stale copy there no longer matters. drivers/lora/loramac-node adds the
# file relative to its own directory, hence the bare name.
set(NB_PATCH_DIR ${CMAKE_CURRENT_LIST_DIR}/../patches/lora)
if(TARGET loramac-node)
  get_target_property(nb_loramac_sources loramac-node SOURCES)
  set(nb_vendored_sx126x ${nb_loramac_sources})
  list(FILTER nb_vendored_sx126x INCLUDE REGEX "(^|/drivers/lora/loramac-node/)sx126x\\.c$")
  list(LENGTH nb_vendored_sx126x nb_vendored_count)
  if(NOT nb_vendored_count EQUAL 1)
    message(FATAL_ERROR "patches/lora/sx126x.c: expected one vendored sx126x.c in target "
      "loramac-node, found '${nb_vendored_sx126x}' - re-check after a Zephyr bump")
  endif()
  list(REMOVE_ITEM nb_loramac_sources ${nb_vendored_sx126x})
  set_property(TARGET loramac-node PROPERTY SOURCES
    ${nb_loramac_sources} ${NB_PATCH_DIR}/sx126x.c)
  target_include_directories(loramac-node PRIVATE
    ${ZEPHYR_BASE}/drivers/lora/loramac-node ${NB_PATCH_DIR})
  # RxDone / TxDone stamps and the timed SetTx (twr_gateway.c, twr_radio.c in the P0 image).
  target_compile_definitions(loramac-node PRIVATE NB_TWR_HOOKS=1)
endif()

# 0.2.2: RX sensitivity tuning (docs/problems.md 1, Kconfig.radio_tune).
if(CONFIG_NB_RADIO_TUNE)
  target_sources(app PRIVATE src/radio_tune.c src/radio_tune_hw.c)
  target_include_directories(app PRIVATE ${NB_PATCH_DIR})
endif()

# ---- update through the Hub (plan/control/radio/northbridge_update.md §3, §10) -------------
#
# The app lives in slot0 at 0x0800_0800 (board dts, zephyr,code-partition) behind nbboot, which
# starts it only when the app_valid record matches the image. Every build therefore also makes
#   build/nbboot/nbboot.{elf,bin,hex}       the boot stub, with this build's compiler;
#   build/zephyr/northbridge.{bin,json}     the update image and its manifest (nb_package.py);
#   build/zephyr/app_valid.bin              the 32 B record for that image;
#   build/zephyr/northbridge_jlink.hex      nbboot + app + record,
# and `west flash` programs northbridge_jlink.hex, so a J-Link flash leaves a bootable layout
# (zephyr.hex alone would land in the ROM bootloader: no record).
set(NB_DIR ${CMAKE_CURRENT_LIST_DIR}/..)
set(NB_NBBOOT_SRC ${NB_DIR}/nbboot/src)
target_include_directories(app PRIVATE ${NB_NBBOOT_SRC})

include(ExternalProject)
set(NB_NBBOOT_BUILD ${CMAKE_BINARY_DIR}/nbboot)
ExternalProject_Add(nbboot
  SOURCE_DIR ${NB_DIR}/nbboot
  BINARY_DIR ${NB_NBBOOT_BUILD}
  CMAKE_ARGS
    -DCMAKE_C_COMPILER=${CMAKE_C_COMPILER}
    -DCMAKE_OBJCOPY=${CMAKE_OBJCOPY}
  INSTALL_COMMAND ""
  BUILD_ALWAYS ON
  BUILD_BYPRODUCTS ${NB_NBBOOT_BUILD}/nbboot.bin ${NB_NBBOOT_BUILD}/nbboot.hex
)
add_dependencies(app nbboot)

file(STRINGS ${CMAKE_CURRENT_LIST_DIR}/src/proto.h nb_ver_lines
  REGEX "^#define FW_VERSION_(MAJOR|MINOR|PATCH) [0-9]+$")
foreach(part MAJOR MINOR PATCH)
  # if(... MATCHES) resets CMAKE_MATCH_*, so take the group before testing it.
  string(REGEX MATCH "FW_VERSION_${part} ([0-9]+)" _ "${nb_ver_lines}")
  set(NB_FW_${part} "${CMAKE_MATCH_1}")
  if(NB_FW_${part} STREQUAL "")
    message(FATAL_ERROR "FW_VERSION_${part} not found in src/proto.h")
  endif()
endforeach()
set(NB_FW_VERSION "${NB_FW_MAJOR}.${NB_FW_MINOR}.${NB_FW_PATCH}")

set(NB_OUT ${CMAKE_BINARY_DIR}/zephyr)
# The version and sha this build carries, for nb_package.py run on the build dir by hand or by
# the CI (it reads them from here when --fw-version / --git-sha are not given).
file(WRITE ${NB_OUT}/nb_build.json
  "{\"fw_version\": \"${NB_FW_VERSION}\", \"git_sha\": \"${NB_GIT_SHA}\"}\n")
set(NB_PACKAGE_OUTPUTS
  ${NB_OUT}/northbridge.bin ${NB_OUT}/northbridge.json ${NB_OUT}/app_valid.bin ${NB_JLINK_HEX})
# Runs after the final link: zephyr.bin is a byproduct of the ELF target's post-build steps.
add_custom_command(OUTPUT ${NB_PACKAGE_OUTPUTS}
  COMMAND ${PYTHON_EXECUTABLE} ${NB_DIR}/tools/nb_package.py
    --bin ${NB_OUT}/${KERNEL_BIN_NAME}
    --fw-version ${NB_FW_VERSION}
    --git-sha ${NB_GIT_SHA}
    --out-dir ${NB_OUT}
    --record ${NB_OUT}/app_valid.bin
    --nbboot ${NB_NBBOOT_BUILD}/nbboot.bin
    --jlink-hex ${NB_JLINK_HEX}
  DEPENDS ${logical_target_for_zephyr_elf} ${NB_OUT}/${KERNEL_BIN_NAME} nbboot
    ${NB_NBBOOT_BUILD}/nbboot.bin ${NB_DIR}/tools/nb_package.py ${NB_DIR}/tools/nb_app_valid.py
  COMMENT "nb_package: northbridge.bin, northbridge.json, northbridge_jlink.hex"
)
add_custom_target(nb_package ALL DEPENDS ${NB_PACKAGE_OUTPUTS})
