from pydantic import BaseModel


class Request(BaseModel):
    """
    Request model for communication between Fiber's components.
    Requests are used for communication between Fiber's client and server.

    Attributes:
        uuid (str): A string representation of a UUID1, uniquely identifying this request.
        request (str): Specifies the type of operation or action the client is requesting.
        body (dict | None): Optional dictionary with additional data or parameters for the request.
    """

    uuid: str
    """uuid (str): A string representation of a UUID1, uniquely identifying this request."""
    request: str
    """request (str): Specifies the type of operation or action the client is requesting."""
    body: dict | None = None
    """body (dict | None): Optional dictionary with additional data or parameters for the request."""


class Response(BaseModel):
    """
    A model representing the server's response to a client's request.

    Attributes:
        uuid (str | None): The UUID correlating to the original request, or None if not applicable.
        response (str | None): The main status message or result of the request, or None if there is no specific response message.
        error (bool): Indicates whether an error occurred during the processing of the request.
        body (dict | int | float | str | None): The data payload of the response, which can vary in type based on the request and context.
    """

    uuid: str | None
    """uuid (str | None): The UUID correlating to the original request, or None if not applicable."""
    response: str | None
    """response (str | None): The main status message or result of the request, or None if there is no specific response message."""
    error: bool
    """error (bool): Indicates whether an error occurred during the processing of the request."""
    body: dict | int | float | str | None
    """body (dict | int | float | str | None): The data payload of the response, which can vary in type based on the request and context."""