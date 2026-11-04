# SPDX-License-Identifier: Apache-2.0
cmake_minimum_required(VERSION 3.20.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(northbridge_tower_link_vectors_test)

set(APP_SRC ${CMAKE_CURRENT_SOURCE_DIR}/../../app/src)

# The golden link vectors (#470 C4) are one plain-text file shared with the central
# (portal/crates/control-radio, src/p2p/tower/link_vectors.rs), at the same repo path on
# both branches. It is read in place and turned into a header at build time.
set(TOWER_LINK_VECTORS
  ${CMAKE_CURRENT_SOURCE_DIR}/../../../../plan/control/radio/p2p_tower_link_vectors.txt
  CACHE FILEPATH "TOWER link vector file")
if(NOT EXISTS ${TOWER_LINK_VECTORS})
  message(FATAL_ERROR "no link vector file at ${TOWER_LINK_VECTORS}")
endif()
set(TOWER_LINK_GEN_DIR ${CMAKE_CURRENT_BINARY_DIR}/tower_link)
set(TOWER_LINK_H ${TOWER_LINK_GEN_DIR}/tower_link_vectors.h)
set(TOWER_LINK_TOOL ${CMAKE_CURRENT_SOURCE_DIR}/vectors_to_header.py)
add_custom_command(
  OUTPUT ${TOWER_LINK_H}
  COMMAND ${PYTHON_EXECUTABLE} ${TOWER_LINK_TOOL} ${TOWER_LINK_VECTORS} ${TOWER_LINK_H}
  DEPENDS ${TOWER_LINK_TOOL} ${TOWER_LINK_VECTORS}
  COMMENT "Generating tower_link_vectors.h from the golden link vectors"
)
add_custom_target(tower_link_vectors_header DEPENDS ${TOWER_LINK_H})
add_dependencies(app tower_link_vectors_header)

# The framing and the link codec, plus the core for TWR_NODE_LIST pages and diag.c for
# EVT_LOG; the UART is a stub (src/main.c).
target_sources(app PRIVATE
  src/main.c
  src/fuzz.c
  ${APP_SRC}/hdlc.c
  ${APP_SRC}/twr_link.c
  ${APP_SRC}/twr_gwc.c
  ${APP_SRC}/twr_ackctrl.c
  ${APP_SRC}/twr_evq.c
  ${APP_SRC}/twr_peers.c
  ${APP_SRC}/twr_dlq.c
  ${APP_SRC}/twr_frame.c
  ${APP_SRC}/twr_crypto.c
  ${APP_SRC}/diag.c
)
target_include_directories(app PRIVATE ${APP_SRC} src ${TOWER_LINK_GEN_DIR})
