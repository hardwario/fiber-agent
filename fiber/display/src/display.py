from collections import defaultdict
from dataclasses import dataclass
from PIL import Image
from threading import Thread, Event, Lock
from uuid import uuid1, UUID

from fiber.display.src.widget import Widget


class CallbackNotCallable(Exception):
    def __init__(self):
        super().__init__("Provided callback is not callable")


@dataclass
class ActiveWidget:
    pos_x: int
    pos_y: int

    widget: Widget


class Display:
    _run_thread: Thread
    _stop_thread: Event
    _lock: Lock
    _current_page: int = 0
    _widgets: dict[int, dict[UUID, ActiveWidget]]
    _changed_page: bool
    _fb: Image.Image

    def __init__(self, width: int, height: int):
        self._stop_thread = Event()
        self._lock = Lock()
        self._fb = Image.new("1", (width, height))
        self._widgets = defaultdict(dict)
        self._changed_page = True

    def get_width(self) -> int:
        return self._fb.width

    def get_height(self) -> int:
        return self._fb.height

    def list_pages(self) -> list[int]:
        return list(self._widgets.keys())

    def set_page(self, page: int) -> bool:
        if self._current_page != page:
            self._changed_page = True
        self._current_page = page

    def add_widget(self, widget: Widget, pos_x: int, pos_y: int, page: int) -> UUID:
        uuid = uuid1()

        with self._lock:
            self._widgets[page][uuid] = ActiveWidget(
                pos_x=pos_x, pos_y=pos_y, widget=widget
            )

        return uuid

    def remove_widget(self, uuid: UUID):
        with self._lock:
            page: dict[UUID, ActiveWidget]
            for page in self._widgets:
                page.pop(uuid)

    def draw(self):
        pass

    def run(self):
        needs_to_draw = False

        if self._changed_page:
            self._fb.paste(0, (0, 0, self.get_width(), self.get_height()))

        active: ActiveWidget
        for active in self._widgets[self._current_page].values():
            active.widget.update()

            if active.widget.has_changed() or self._changed_page:
                active.widget.draw()
                self._fb.paste(active.widget.fb, (active.pos_x, active.pos_y))
                needs_to_draw = True

        if needs_to_draw or self._changed_page:
            self._changed_page = False
            self.draw()
