import PIL.Image as Image


class Widget:
    _changed: bool = True
    width: int
    height: int
    fb: Image.Image

    def __init__(self, width: int, height: int):
        self.fb = Image.new('1', (width, height))
        self._changed = True

    def get_width(self) -> int:
        return self.fb.width

    def get_height(self) -> int:
        return self.fb.height

    def update(self):
        pass

    def draw(self):
        pass

    def has_changed(self) -> bool:
        changed = self._changed
        self._changed = False
        return changed
