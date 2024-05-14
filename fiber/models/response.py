from pydantic import BaseModel


class Response(BaseModel):
    '''
    A model representing the server's response to a client's request.

    Attributes:
        uuid (str | None): The UUID correlating to the original request, or None if not applicable.
        response (str | None): The main status message or result of the request, or None if there is no specific response message.
        error (bool): Indicates whether an error occurred during the processing of the request.
        body (dict | int | float | str | None): The data payload of the response, which can vary in type based on the request and context.
    '''

    uuid: str | None
    '''uuid (str | None): The UUID correlating to the original request, or None if not applicable.'''
    response: str | None
    '''response (str | None): The main status message or result of the request, or None if there is no specific response message.'''
    error: bool
    '''error (bool): Indicates whether an error occurred during the processing of the request.'''
    body: dict | int | float | str | None
    '''body (dict | int | float | str | None): The data payload of the response, which can vary in type based on the request and context.'''