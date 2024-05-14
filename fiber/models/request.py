from pydantic import BaseModel


class Request(BaseModel):
    '''
    Request model for communication between Fiber's components.
    Requests are used for communication between Fiber's client and server.

    Attributes:
        uuid (str): A string representation of a UUID1, uniquely identifying this request.
        request (str): Specifies the type of operation or action the client is requesting.
        body (dict | None): Optional dictionary with additional data or parameters for the request.
    '''

    uuid: str
    '''uuid (str): A string representation of a UUID1, uniquely identifying this request.'''
    request: str
    '''request (str): Specifies the type of operation or action the client is requesting.'''
    body: dict | None = None
    '''body (dict | None): Optional dictionary with additional data or parameters for the request.'''
