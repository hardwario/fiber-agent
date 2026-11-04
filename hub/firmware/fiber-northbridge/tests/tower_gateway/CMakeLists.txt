# SPDX-License-Identifier: Apache-2.0
cmake_minimum_required(VERSION 3.20.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(northbridge_tower_gateway_test)

set(APP_SRC ${CMAKE_CURRENT_SOURCE_DIR}/../../app/src)

# The same KAT files as tests/tower_codec, byte for byte: its ccm/ directory and its loader
# (kat_to_header.py) are used in place, not copied.
set(TOWER_KAT_DIR ${CMAKE_CURRENT_SOURCE_DIR}/../tower_codec/ccm CACHE PATH
  "Directory of tower_*_kat.json")
file(GLOB TOWER_KAT_FILES CONFIGURE_DEPENDS ${TOWER_KAT_DIR}/tower_*_kat.json)
if(NOT TOWER_KAT_FILES)
  message(FATAL_ERROR "no tower_*_kat.json in ${TOWER_KAT_DIR}")
endif()
set(TOWER_KAT_GEN_DIR ${CMAKE_CURRENT_BINARY_DIR}/tower_kat)
set(TOWER_KAT_H ${TOWER_KAT_GEN_DIR}/tower_kat.h)
set(TOWER_KAT_TOOL ${CMAKE_CURRENT_SOURCE_DIR}/../tower_codec/kat_to_header.py)
add_custom_command(
  OUTPUT ${TOWER_KAT_H}
  COMMAND ${PYTHON_EXECUTABLE} ${TOWER_KAT_TOOL} ${TOWER_KAT_H} ${TOWER_KAT_FILES}
  DEPENDS ${TOWER_KAT_TOOL} ${TOWER_KAT_FILES}
  COMMENT "Generating tower_kat.h from TOWER KAT JSON"
)
add_custom_target(tower_kat_header DEPENDS ${TOWER_KAT_H})
add_dependencies(app tower_kat_header)

# The ACK CTRL tail KAT (#470 §13.5), shared verbatim with the STICKER node; one line per
# vector, turned into a header with the link vector tool.
set(ACK_CTRL_KAT
  ${CMAKE_CURRENT_SOURCE_DIR}/../../../../plan/control/radio/p2p_tower_ack_ctrl_kat.txt
  CACHE FILEPATH "TOWER ACK CTRL KAT file")
if(NOT EXISTS ${ACK_CTRL_KAT})
  message(FATAL_ERROR "no ACK CTRL KAT at ${ACK_CTRL_KAT}")
endif()
set(ACK_CTRL_KAT_H ${TOWER_KAT_GEN_DIR}/ack_ctrl_kat.h)
set(ACK_CTRL_KAT_TOOL ${CMAKE_CURRENT_SOURCE_DIR}/../tower_link_vectors/vectors_to_header.py)
add_custom_command(
  OUTPUT ${ACK_CTRL_KAT_H}
  COMMAND ${PYTHON_EXECUTABLE} ${ACK_CTRL_KAT_TOOL} ${ACK_CTRL_KAT} ${ACK_CTRL_KAT_H} ack_ctrl_kat
  DEPENDS ${ACK_CTRL_KAT_TOOL} ${ACK_CTRL_KAT}
  COMMENT "Generating ack_ctrl_kat.h from the ACK CTRL KAT"
)
add_custom_target(ack_ctrl_kat_header DEPENDS ${ACK_CTRL_KAT_H})
add_dependencies(app ack_ctrl_kat_header)

# The radio-free core and the link codec only; the command path (cmd_dispatch.c, twr_cmd.c)
# is tests/cmd_dispatch against fakes of the radio runtime.
target_sources(app PRIVATE
  src/main.c
  src/test_link.c
  src/test_ack_ctrl.c
  ${APP_SRC}/twr_crypto.c
  ${APP_SRC}/twr_frame.c
  ${APP_SRC}/twr_peers.c
  ${APP_SRC}/twr_dlq.c
  ${APP_SRC}/twr_gwc.c
  ${APP_SRC}/twr_ackctrl.c
  ${APP_SRC}/twr_airtime.c
  ${APP_SRC}/twr_evq.c
  ${APP_SRC}/twr_link.c
)
target_include_directories(app PRIVATE ${APP_SRC} src ${TOWER_KAT_GEN_DIR})
