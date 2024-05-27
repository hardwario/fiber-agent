from pydantic import BaseModel, validator
from fiber.common.consts import VALID_PROBES


class SensorDisplayBody(BaseModel):
    '''
    Represents the body for modifying the sensor display

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
