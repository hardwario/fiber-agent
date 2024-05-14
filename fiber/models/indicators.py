from pydantic import BaseModel, validator
from fiber.common.consts import VALID_PROBES

class StateIndicatorBody(BaseModel):
    '''
    Represents an indicator state for a specific probe.

    Attributes:
        output (int): The probe number.
        state (bool): The state of the indicator.
    '''

    output: int
    '''The probe number.'''
    state: bool
    '''The state of the indicator.'''

    @validator('output')
    def validate_output(cls, value):
        '''
        Validates the output field.

        Raises:
            ValueError: If output is not a valid probe number.
        '''
        if value not in VALID_PROBES:
            raise ValueError('Invalid probe')
        return value


class ColorIndicatorBody(BaseModel):
    '''
    Represents the color of an indicator for a specific probe.

    Attributes:
        output (int): The probe number.
        temperature (int | float | None): The temperature value to determine the indicator color.
            If None, the indicator color is set to red.
            If a int or float value is provided, the indicator color is set to green.
    '''

    output: int
    '''The probe number.'''
    temperature: int | float | None
    '''The temperature value to determine the indicator color.'''

    @validator('output')
    def validate_output(cls, value):
        '''
        Validates the output field.

        Raises:
            ValueError: If the value is not a valid probe number.
        '''
        if value not in VALID_PROBES:
            raise ValueError('Invalid probe')
        return value
