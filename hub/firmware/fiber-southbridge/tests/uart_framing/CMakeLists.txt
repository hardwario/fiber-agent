# SPDX-License-Identifier: Apache-2.0
cmake_minimum_required(VERSION 3.20.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(southbridge_uart_framing_test)

set(APP_SRC ${CMAKE_CURRENT_SOURCE_DIR}/../../app/src)

target_sources(app PRIVATE
  src/main.c
  ${APP_SRC}/uart_proto.c
)
target_include_directories(app PRIVATE ${APP_SRC})
