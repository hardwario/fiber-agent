from loguru import logger

from fiber.client.manager import InterfaceManager
from fiber.models.indicators import StateIndicatorBody
from fiber.models.display_sensor import SensorDisplayBody
from fiber.models.system import FiberIdBody, RebootBody


class InterfaceHandler(InterfaceManager):
    def get_mac(self) -> str:
        mac = self.get_response(operation='get_mac')
        return mac

    def get_ip(self) -> str:
        ip = self.get_response(operation='get_ip')
        return ip

    def get_fiber_id(self) -> int:
        fiber_id = self.get_response(operation='get_fiber_id')
        return fiber_id

    def get_uptime(self) -> float:
        uptime = self.get_response(operation='get_uptime')
        return uptime

    def set_indicator_state(self, probe: int, state: bool) -> None:
        '''
        Sets the state of the indicator for a specific probe.

        Args:
            probe (int): The probe number.
            state (bool): The state to set for the indicator.

        Returns:
            None
        '''
        logger.debug(f'Probe: {probe}, State: {state}')
        state_body = StateIndicatorBody(output=probe, state=state)
        self.send_request(operation='set_indicator_state',
                          payload=dict(state_body))

    def update_sensor_display(self, probe: int, temperature: int | float | None) -> None:
        '''
        Sets the color of the indicator based on the temperature value.
        If temperature is None, the indicator color is set to red.
        If a float value is provided, the indicator color is set to green.

        Args:
            probe (int): The probe number.
            temperature (float | None): The temperature value to determine the indicator color.
        Returns:
            None
        '''
        logger.debug(f'Probe: {probe}, Temperature: {temperature}')
        color_body = SensorDisplayBody(output=probe, temperature=temperature)
        self.send_request(operation='update_sensor_display',
                          payload=dict(color_body))

    def set_fiber_id(self, fiber_id: int) -> None:
        id_body = FiberIdBody(id=fiber_id)
        self.send_request(operation='set_id', payload=dict(id_body))

    def reboot(self, delay: int = 0) -> None:
        reboot_body = RebootBody(delay=delay)
        self.send_request(operation='reboot', payload=reboot_body.model_dump())
