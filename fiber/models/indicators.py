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


