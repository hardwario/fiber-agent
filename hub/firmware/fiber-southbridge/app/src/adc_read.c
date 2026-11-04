/* SPDX-License-Identifier: Apache-2.0 */
#include "adc_read.h"
#include "uart_proto.h"

#include <errno.h>
#include <string.h>
#include <stdio.h>
#include <zephyr/devicetree.h>
#include <zephyr/drivers/adc.h>

#define ADC_USER_NODE DT_PATH(zephyr_user)

static const struct adc_dt_spec adc_vbat = ADC_DT_SPEC_GET_BY_IDX(ADC_USER_NODE, 0);
static const struct adc_dt_spec adc_vin  = ADC_DT_SPEC_GET_BY_IDX(ADC_USER_NODE, 1);

static int read_raw(const struct adc_dt_spec *spec, uint16_t *raw_out)
{
	int16_t sample = 0;
	struct adc_sequence seq = {
		.buffer = &sample,
		.buffer_size = sizeof(sample),
	};

	int rc = adc_sequence_init_dt(spec, &seq);
	if (rc < 0) {
		return rc;
	}

	rc = adc_read(spec->dev, &seq);
	if (rc < 0) {
		return rc;
	}

	*raw_out = (uint16_t)sample;
	return 0;
}

int adc_read_init(void)
{
	if (!adc_is_ready_dt(&adc_vbat) || !adc_is_ready_dt(&adc_vin)) {
		return -ENODEV;
	}

	int rc = adc_channel_setup_dt(&adc_vbat);
	if (rc < 0) {
		return rc;
	}
	rc = adc_channel_setup_dt(&adc_vin);
	if (rc < 0) {
		return rc;
	}
	return 0;
}

int adc_read_print(const char *channel_name)
{
	const struct adc_dt_spec *spec;
	bool is_vin;

	if (strcmp(channel_name, "VIN") == 0) {
		spec = &adc_vin;
		is_vin = true;
	} else if (strcmp(channel_name, "VBAT") == 0) {
		spec = &adc_vbat;
		is_vin = false;
	} else {
		uart_proto_write("ERR: unknown ADC name (use VIN or VBAT)\r\n");
		return -EINVAL;
	}

	uint16_t raw = 0;
	int rc = read_raw(spec, &raw);
	if (rc < 0) {
		uart_proto_write("ERR: adc read failed\r\n");
		return rc;
	}

	/* Pin voltage in mV: raw * 3300 / 4095 (integer, matches legacy). */
	int32_t v_pin_mv = (int32_t)raw * 3300 / 4095;
	/* Real voltage after divider:
	 *   VIN  = v_pin * 398 / 68    ≈ (330k+68k)/68k = 5.85
	 *   VBAT = v_pin * 2           = (68k+68k)/68k  = 2.0
	 */
	int32_t v_real_mv = is_vin ? (v_pin_mv * 398 / 68) : (v_pin_mv * 2);

	char buf[96];
	int n = snprintf(buf, sizeof(buf),
			 "%s_raw=%u  %s_pin=%ldmV  %s=%ldmV\r\n",
			 channel_name, (unsigned)raw,
			 channel_name, (long)v_pin_mv,
			 channel_name, (long)v_real_mv);
	if (n > 0) {
		uart_proto_write_bytes(buf, (size_t)n);
	}
	return 0;
}
