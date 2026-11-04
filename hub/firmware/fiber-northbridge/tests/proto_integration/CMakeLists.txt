# SPDX-License-Identifier: Apache-2.0
cmake_minimum_required(VERSION 3.20.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(northbridge_proto_integration_test)

set(APP_SRC ${CMAKE_CURRENT_SOURCE_DIR}/../../app/src)
set(FAKES_DIR ${CMAKE_CURRENT_SOURCE_DIR}/../cmd_dispatch/src)

# The real HDLC decoder in front of the real command path, against the same fakes of the
# radio runtime and the UART as tests/cmd_dispatch.
target_sources(app PRIVATE
  src/main.c
  ${FAKES_DIR}/gw_fakes.c
  ${APP_SRC}/hdlc.c
  ${APP_SRC}/cmd_dispatch.c
  ${APP_SRC}/twr_cmd.c
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
target_include_directories(app PRIVATE ${APP_SRC} ${FAKES_DIR})
