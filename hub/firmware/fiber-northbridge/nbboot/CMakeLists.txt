# SPDX-License-Identifier: Apache-2.0
#
# nbboot, the Northbridge boot stub (src/nbboot.c): bare metal, built with the Zephyr SDK's
# arm-zephyr-eabi-gcc but without Zephyr. app/CMakeLists.txt builds it as an external project
# of every app build (build/nbboot/), with the app's compiler. Standalone:
#
#   cmake -S fiber-northbridge/nbboot -B build-nbboot -G Ninja
#   cmake --build build-nbboot          # nbboot.elf, nbboot.bin, nbboot.hex
#
# The compiler comes from -DCMAKE_C_COMPILER=... or else the SDK under ZEPHYR_SDK_INSTALL_DIR.
cmake_minimum_required(VERSION 3.20.0)

set(CMAKE_SYSTEM_NAME Generic)
set(CMAKE_SYSTEM_PROCESSOR arm)
set(CMAKE_TRY_COMPILE_TARGET_TYPE STATIC_LIBRARY)
if(NOT CMAKE_C_COMPILER)
  if(NOT DEFINED ENV{ZEPHYR_SDK_INSTALL_DIR})
    message(FATAL_ERROR "nbboot: pass -DCMAKE_C_COMPILER=<arm-zephyr-eabi-gcc> "
      "or set ZEPHYR_SDK_INSTALL_DIR")
  endif()
  set(CMAKE_C_COMPILER
    $ENV{ZEPHYR_SDK_INSTALL_DIR}/gnu/arm-zephyr-eabi/bin/arm-zephyr-eabi-gcc)
endif()

project(nbboot C)

add_executable(nbboot.elf src/nbboot.c src/nb_app_valid.c)
target_include_directories(nbboot.elf PRIVATE src)
target_compile_options(nbboot.elf PRIVATE
  -mcpu=cortex-m4 -mthumb -mfloat-abi=soft
  -Os -ffreestanding -fno-common -ffunction-sections -fdata-sections
  -Wall -Wextra -Werror -std=c11
)
target_link_options(nbboot.elf PRIVATE
  -mcpu=cortex-m4 -mthumb -mfloat-abi=soft
  -nostdlib -nostartfiles
  -T${CMAKE_CURRENT_SOURCE_DIR}/nbboot.ld
  -Wl,--gc-sections
  -Wl,-Map=${CMAKE_CURRENT_BINARY_DIR}/nbboot.map
  -Wl,--print-memory-usage
)
set_target_properties(nbboot.elf PROPERTIES LINK_DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/nbboot.ld)

add_custom_command(TARGET nbboot.elf POST_BUILD
  COMMAND ${CMAKE_OBJCOPY} -O binary nbboot.elf nbboot.bin
  COMMAND ${CMAKE_OBJCOPY} -O ihex nbboot.elf nbboot.hex
  BYPRODUCTS ${CMAKE_CURRENT_BINARY_DIR}/nbboot.bin ${CMAKE_CURRENT_BINARY_DIR}/nbboot.hex
  WORKING_DIRECTORY ${CMAKE_CURRENT_BINARY_DIR}
  COMMENT "nbboot.bin, nbboot.hex"
)
