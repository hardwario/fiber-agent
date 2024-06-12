import re
from pydantic import BaseModel, field_validator

    
class BeaconBody(BaseModel):
    '''
    Represents the beacon data.

    Attributes:
        uptime (int): The uptime of the system.
        ip_address (str): The IP address of the system.
        mac_address (str): The MAC address of the system.

    '''

    uptime: int | float | None
    '''The uptime of the system.'''

    ip_address: str | None
    '''The IP address of the system.'''

    mac_address: str | None
    '''The MAC address of the system.'''

    @field_validator('mac_address')
    def validate_mac_address(cls, value):
        '''
        Validates the mac_address field.

        Raises:
            ValueError: If the value is not a valid MAC address.
        '''
        if value is None:
            return value
        if not re.match(r'^([0-9A-Fa-f]{2}[:-]){5}([0-9A-Fa-f]{2})$', value):
            raise ValueError('Invalid MAC address format')
        return value
    
    @field_validator('uptime')
    def validate_uptime(cls, value):
        '''
        Validates the uptime field.

        Raises:
            ValueError: If the value is not a valid uptime.
        '''
        if value is None:
            return value
        if not isinstance(value, (int, float)):
            raise ValueError('Uptime must be a non-negative integer or float')
        return value


class FiberIdBody(BaseModel):
    '''
    Represents the fiber ID.

    Attributes:
        id (int): The fiber ID.

    '''

    id: int
    '''The fiber ID.'''

    @field_validator('id')
    def validate_id(cls, value):
        '''
        Validates the id field.

        Raises:
            ValueError: If the value is not within the valid range of 2159017983 >= id >= 2157969408.
        '''
        if not (2159017983 >= value >= 2157969408):
            raise ValueError('ID out of range')
        return value


class RebootBody(BaseModel):
    '''
    Represents the reboot data.

    Attributes:
        delay (int): The delay in seconds before rebooting.

    '''

    delay: int | float
    '''The delay in seconds before rebooting.'''

    @field_validator('delay')
    def validate_delay(cls, value):
        '''
        Validates the delay field.

        Raises:
            ValueError: If the value is not a non-negative integer or float.
        '''
        if not isinstance(value, (int, float)):
            raise ValueError('Delay must be a non-negative integer or float')
        return value