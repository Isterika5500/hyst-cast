"""Рисует иконку hyst-cast: скруглённый квадрат с градиентом (индиго → фиолетовый) и треугольник «эфир».
python scripts/make_icon.py → client/assets/icon.png (256) и client/assets/icon.ico (16…256)"""
from PIL import Image, ImageDraw, ImageFilter

S = 1024  # рисуем крупно и уменьшаем — так края гладкие
a, b = (0x7C, 0x7F, 0xF5), (0xBF, 0x5A, 0xF2)
grad = Image.new("RGBA", (S, S))
px = grad.load()
for y in range(S):
    for x in range(S):
        t = (x + y) / (2 * S - 2)
        px[x, y] = tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3)) + (255,)
mask = Image.new("L", (S, S), 0)
pad = 56
ImageDraw.Draw(mask).rounded_rectangle([pad, pad, S - pad, S - pad], radius=230, fill=255)
icon = Image.new("RGBA", (S, S), (0, 0, 0, 0))
icon.paste(grad, (0, 0), mask)
# мягкий блик сверху
gloss = Image.new("RGBA", (S, S), (0, 0, 0, 0))
ImageDraw.Draw(gloss).ellipse([-S * 0.2, -S * 0.75, S * 1.2, S * 0.42], fill=(255, 255, 255, 34))
icon = Image.alpha_composite(icon, Image.composite(gloss, Image.new("RGBA", (S, S)), mask))
# треугольник «play» со скруглением (через размытие маски)
tri = Image.new("L", (S, S), 0)
c, r = S / 2 + 40, 215
ImageDraw.Draw(tri).polygon([(c - r * 0.75, S / 2 - r), (c - r * 0.75, S / 2 + r), (c + r * 0.95, S / 2)], fill=255)
tri = tri.filter(ImageFilter.GaussianBlur(14)).point(lambda v: 255 if v > 128 else 0)
white = Image.new("RGBA", (S, S), (255, 255, 255, 255))
icon.paste(white, (0, 0), tri)
icon.resize((256, 256), Image.LANCZOS).save("client/assets/icon.png")
icon.save("client/assets/icon.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
print("готово")
