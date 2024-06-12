from pydantic import BaseModel, field_validator

class VoltageBody(BaseModel):
    '''
    Represents the voltage data.

    Attributes:
        battery_voltage (int | float): The battery voltage.
        poe_voltage (int | float): The PoE voltage.
    '''

    battery_voltage: int | float
    '''The battery voltage.'''

    poe_voltage: int | float
    '''The PoE voltage.'''

    @field_validator('battery_voltage')
    def validate_battery_voltage(cls, value):
        '''
        Validates the battery_voltage field.

        Raises:
            ValueError: If the value is not a valid battery voltage.
        '''
        if not isinstance(value, (int, float)):
            raise ValueError('Battery voltage must be a number')
        return value
    
    @field_validator('poe_voltage')
    def validate_poe_voltage(cls, value):
        '''
        Validates the poe_voltage field.

        Raises:
            ValueError: If the value is not a valid PoE voltage.
        '''
        if not isinstance(value, (int, float)):
            raise ValueError('PoE voltage must be a number')
        return value