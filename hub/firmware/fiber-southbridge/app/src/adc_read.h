/* SPDX-License-Identifier: Apache-2.0 */
#ifndef APP_ADC_READ_H_
#define APP_ADC_READ_H_

#include <stdint.h>

int adc_read_init(void);

/* Reads one shot from VIN or VBAT, emits the formatted line via
 * uart_proto_write(). Returns 0 on success.
 * channel_name must be "VIN" or "VBAT".
 */
int adc_read_print(const char *channel_name);

#endif /* APP_ADC_READ_H_ */
