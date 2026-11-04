# SPDX-License-Identifier: Apache-2.0
cmake_minimum_required(VERSION 3.20.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(northbridge_tower_codec_test)

set(APP_SRC ${CMAKE_CURRENT_SOURCE_DIR}/../../app/src)

# KAT loader seam: every tower_*_kat.json in TOWER_KAT_DIR (default: the copies in ccm/)
# becomes one C header at build time, so a new planner KAT only has to be dropped in.
set(TOWER_KAT_DIR ${CMAKE_CURRENT_SOURCE_DIR}/ccm CACHE PATH "Directory of tower_*_kat.json")
file(GLOB TOWER_KAT_FILES CONFIGURE_DEPENDS ${TOWER_KAT_DIR}/tower_*_kat.json)
if(NOT TOWER_KAT_FILES)
  message(FATAL_ERROR "no tower_*_kat.json in ${TOWER_KAT_DIR}")
endif()
set(TOWER_KAT_GEN_DIR ${CMAKE_CURRENT_BINARY_DIR}/tower_kat)
set(TOWER_KAT_H ${TOWER_KAT_GEN_DIR}/tower_kat.h)
add_custom_command(
  OUTPUT ${TOWER_KAT_H}
  COMMAND ${PYTHON_EXECUTABLE} ${CMAKE_CURRENT_SOURCE_DIR}/kat_to_header.py
          ${TOWER_KAT_H} ${TOWER_KAT_FILES}
  DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/kat_to_header.py ${TOWER_KAT_FILES}
  COMMENT "Generating tower_kat.h from TOWER KAT JSON"
)
add_custom_target(tower_kat_header DEPENDS ${TOWER_KAT_H})
add_dependencies(app tower_kat_header)

target_sources(app PRIVATE
  src/main.c
  ${APP_SRC}/twr_crypto.c
  ${APP_SRC}/twr_frame.c
  ${APP_SRC}/twr_gw.c
  ${APP_SRC}/twr_airtime.c
)
target_include_directories(app PRIVATE ${APP_SRC} src ${TOWER_KAT_GEN_DIR})
