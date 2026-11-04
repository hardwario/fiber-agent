# SPDX-License-Identifier: Apache-2.0
cmake_minimum_required(VERSION 3.20.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(northbridge_nbboot_record_test)

# The boot stub's record check and boot decision, the very file nbboot links (and the app reads
# GET_INFO's tail with), over plain buffers instead of flash.
set(NBBOOT_SRC ${CMAKE_CURRENT_SOURCE_DIR}/../../nbboot/src)

target_sources(app PRIVATE
  src/main.c
  ${NBBOOT_SRC}/nb_app_valid.c
)
target_include_directories(app PRIVATE ${NBBOOT_SRC})
