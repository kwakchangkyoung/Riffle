"""Riffle 앱 아이콘 생성 (1024x1024 PNG).

macOS Big Sur 이후 아이콘 그리드: 1024 캔버스 안에 824x824 둥근 사각형(여백 100px),
모서리 반경 약 185px. 4배 슈퍼샘플링 후 축소해 안티앨리어싱한다.
"""

import sys

from PIL import Image, ImageDraw, ImageFilter

S = 4  # 슈퍼샘플링 배율
W = 1024 * S


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(len(a)))


def main(out_path: str) -> None:
    img = Image.new("RGBA", (W, W), (0, 0, 0, 0))

    # 둥근 사각형 영역
    margin = 100 * S
    box = (margin, margin, W - margin, W - margin)
    radius = 185 * S

    # 그림자
    shadow = Image.new("RGBA", (W, W), (0, 0, 0, 0))
    ImageDraw.Draw(shadow).rounded_rectangle(
        (box[0], box[1] + 18 * S, box[2], box[3] + 18 * S), radius, fill=(0, 0, 0, 110)
    )
    shadow = shadow.filter(ImageFilter.GaussianBlur(28 * S))
    img.alpha_composite(shadow)

    # 세로 그라디언트 배경 (짙은 남색 → 청록빛 남색)
    top, bottom = (24, 32, 56), (14, 58, 82)
    grad = Image.new("RGBA", (W, W))
    gd = ImageDraw.Draw(grad)
    for y in range(W):
        gd.line([(0, y), (W, y)], fill=lerp(top, bottom, y / W) + (255,))
    mask = Image.new("L", (W, W), 0)
    ImageDraw.Draw(mask).rounded_rectangle(box, radius, fill=255)
    img.paste(grad, (0, 0), mask)

    # 반투명 도형은 별도 레이어에 그린 뒤 합성한다 (직접 그리면 알파가 덮어써짐)
    layer = Image.new("RGBA", (W, W), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)

    # 로그 줄을 표현하는 가로 막대들 (왼쪽 거터 점 + 본문 막대)
    left = margin + 120 * S
    gutter_w = 46 * S
    body_left = left + gutter_w + 40 * S
    right_max = W - margin - 120 * S
    bar_h = 44 * S
    gap = 30 * S
    lengths = [0.92, 0.70, 0.84, 0.55, 0.96, 0.62, 0.78]
    colors = [
        (120, 160, 210),  # INFO 파랑 계열
        (120, 160, 210),
        (236, 104, 96),   # ERROR 빨강
        (120, 160, 210),
        (120, 160, 210),  # 하이라이트 줄 (아래에서 덮음)
        (245, 166, 70),   # WARN 주황
        (120, 160, 210),
    ]
    n = len(lengths)
    total_h = n * bar_h + (n - 1) * gap
    y0 = (W - total_h) // 2
    highlight_index = 4

    for i, (frac, color) in enumerate(zip(lengths, colors)):
        y = y0 + i * (bar_h + gap)
        if i == highlight_index:
            # 검색 하이라이트 줄: 줄 전체 배경 띠 + 노란 막대
            d.rounded_rectangle(
                (margin + 70 * S, y - 16 * S, W - margin - 100 * S, y + bar_h + 16 * S),
                26 * S,
                fill=(255, 214, 92, 42),
            )
            color = (255, 214, 92)
        # 거터(줄 번호) 점
        d.rounded_rectangle(
            (left, y + 8 * S, left + gutter_w, y + bar_h - 8 * S),
            12 * S,
            fill=(255, 255, 255, 70),
        )
        # 본문 막대
        right = body_left + int((right_max - body_left) * frac)
        d.rounded_rectangle((body_left, y, right, y + bar_h), bar_h // 2, fill=color + (255,))

    # 오른쪽 커스텀 스크롤바 + thumb
    sb_x = W - margin - 70 * S
    d.rounded_rectangle(
        (sb_x, margin + 130 * S, sb_x + 18 * S, W - margin - 130 * S), 9 * S, fill=(255, 255, 255, 40)
    )
    d.rounded_rectangle(
        (sb_x, margin + 330 * S, sb_x + 18 * S, margin + 470 * S), 9 * S, fill=(255, 255, 255, 170)
    )

    img.alpha_composite(layer)

    # 위쪽 광택
    gloss = Image.new("RGBA", (W, W), (0, 0, 0, 0))
    ImageDraw.Draw(gloss).rounded_rectangle(box, radius, outline=(255, 255, 255, 50), width=4 * S)
    img.alpha_composite(gloss)

    img = img.resize((1024, 1024), Image.LANCZOS)
    img.save(out_path)


if __name__ == "__main__":
    main(sys.argv[1])
