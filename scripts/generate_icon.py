#!/usr/bin/env python3
"""
Generate a professional Apple HIG-style squircle app icon for Command Code.
Produces:
- src-tauri/icons/icon.png (512x512)
- src-tauri/icons/32x32.png
- src-tauri/icons/128x128.png
- src-tauri/icons/128x128@2x.png (256x256)
- src-tauri/icons/icon.icns (via iconutil)
- src-tauri/icons/Command Code.icns
"""

import os
import subprocess
import tempfile
from PIL import Image, ImageDraw, ImageFilter

SIZE = 1024

def create_master_icon():
    img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    
    # 1. Base squircle bounds (macOS icon grid)
    left, top, right, bottom = 100, 100, 924, 924
    corner_radius = 185
    
    # Mask
    mask = Image.new("L", (SIZE, SIZE), 0)
    draw_mask = ImageDraw.Draw(mask)
    draw_mask.rounded_rectangle([left, top, right, bottom], radius=corner_radius, fill=255)
    
    # 2. Outer drop shadow
    shadow = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    for offset, blur, alpha in [(24, 32, 70), (12, 16, 100), (6, 8, 120)]:
        s = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
        sd = ImageDraw.Draw(s)
        sd.rounded_rectangle([left, top + offset, right, bottom + offset], radius=corner_radius, fill=(0, 0, 0, alpha))
        s = s.filter(ImageFilter.GaussianBlur(blur))
        shadow = Image.alpha_composite(shadow, s)
        
    img = Image.alpha_composite(img, shadow)

    # 3. Squircle body: Deep space metallic gradient
    body = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    body_draw = ImageDraw.Draw(body)
    
    for y in range(top, bottom):
        ratio = (y - top) / float(bottom - top)
        r = int(18 * (1 - ratio) + 8 * ratio)
        g = int(24 * (1 - ratio) + 12 * ratio)
        b = int(42 * (1 - ratio) + 24 * ratio)
        body_draw.line([(left, y), (right, y)], fill=(r, g, b, 255))
        
    # Ambient inner glow at center
    glow = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    glow_draw = ImageDraw.Draw(glow)
    cx, cy = SIZE // 2, SIZE // 2 - 20
    for rad in range(350, 0, -5):
        alpha = int(35 * (1 - rad / 350.0))
        glow_draw.ellipse([cx - rad, cy - rad, cx + rad, cy + rad], fill=(0, 180, 255, alpha))
    glow = glow.filter(ImageFilter.GaussianBlur(40))
    body = Image.alpha_composite(body, glow)

    # Inner subtle specular highlight
    highlight = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    hl_draw = ImageDraw.Draw(highlight)
    hl_draw.rounded_rectangle([left + 2, top + 2, right - 2, bottom - 2], radius=corner_radius - 2, outline=(255, 255, 255, 45), width=2)
    hl_draw.arc([left + 2, bottom - 2 * corner_radius, right - 2, bottom - 2], start=0, end=180, fill=(0, 0, 0, 90), width=3)
    body = Image.alpha_composite(body, highlight)

    body.putalpha(mask)
    img = Image.alpha_composite(img, body)

    # 4. Central Motif: Glowing Neon Terminal Prompt `>` and Code Cursor `_`
    chevron_pts = [(300, 370), (450, 500), (300, 630)]
    cursor_pts = [(500, 630), (730, 630)]
    
    # Neon bloom
    bloom = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    bloom_draw = ImageDraw.Draw(bloom)
    bloom_draw.line(chevron_pts, fill=(0, 240, 255, 180), width=74, joint="curve")
    bloom_draw.line(cursor_pts, fill=(236, 72, 153, 180), width=74)
    bloom_draw.ellipse([670, 410, 750, 490], fill=(168, 85, 247, 180))
    bloom = bloom.filter(ImageFilter.GaussianBlur(32))
    img = Image.alpha_composite(img, bloom)
    
    bloom2 = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    b2_draw = ImageDraw.Draw(bloom2)
    b2_draw.line(chevron_pts, fill=(0, 255, 255, 220), width=62, joint="curve")
    b2_draw.line(cursor_pts, fill=(244, 114, 182, 220), width=62)
    b2_draw.ellipse([680, 420, 740, 480], fill=(192, 132, 252, 220))
    bloom2 = bloom2.filter(ImageFilter.GaussianBlur(12))
    img = Image.alpha_composite(img, bloom2)

    # Core sharp lines
    core = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    core_draw = ImageDraw.Draw(core)
    core_draw.line(chevron_pts, fill=(224, 254, 255, 255), width=52, joint="curve")
    core_draw.line(cursor_pts, fill=(253, 242, 248, 255), width=52)
    core_draw.ellipse([690, 430, 730, 470], fill=(255, 255, 255, 255))
    
    fiber = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    fiber_draw = ImageDraw.Draw(fiber)
    fiber_draw.arc([430, 390, 710, 610], start=220, end=350, fill=(56, 189, 248, 110), width=10)
    fiber = fiber.filter(ImageFilter.GaussianBlur(2))
    
    img = Image.alpha_composite(img, fiber)
    img = Image.alpha_composite(img, core)
    
    return img

def main():
    icons_dir = os.path.abspath("src-tauri/icons")
    os.makedirs(icons_dir, exist_ok=True)
    
    print("🎨 Rendering 1024x1024 master icon...")
    master = create_master_icon()
    
    master_path = os.path.join(icons_dir, "icon-1024.png")
    master.save(master_path, "PNG")
    
    sizes = {
        "icon.png": (512, 512),
        "128x128@2x.png": (256, 256),
        "128x128.png": (128, 128),
        "32x32.png": (32, 32),
    }
    
    for name, (w, h) in sizes.items():
        resized = master.resize((w, h), Image.Resampling.LANCZOS)
        out_path = os.path.join(icons_dir, name)
        resized.save(out_path, "PNG")
        print(f"  ✓ Generated {name} ({w}x{h})")
        
    with tempfile.TemporaryDirectory() as tmpdir:
        iconset_dir = os.path.join(tmpdir, "icons.iconset")
        os.makedirs(iconset_dir, exist_ok=True)
        
        iconset_sizes = [
            ("icon_16x16.png", 16),
            ("icon_16x16@2x.png", 32),
            ("icon_32x32.png", 32),
            ("icon_32x32@2x.png", 64),
            ("icon_128x128.png", 128),
            ("icon_128x128@2x.png", 256),
            ("icon_256x256.png", 256),
            ("icon_256x256@2x.png", 512),
            ("icon_512x512.png", 512),
            ("icon_512x512@2x.png", 1024),
        ]
        
        for fname, s in iconset_sizes:
            resized = master.resize((s, s), Image.Resampling.LANCZOS)
            resized.save(os.path.join(iconset_dir, fname), "PNG")
            
        icns_path = os.path.join(icons_dir, "icon.icns")
        cmd_code_icns_path = os.path.join(icons_dir, "Command Code.icns")
        subprocess.run(["iconutil", "-c", "icns", iconset_dir, "-o", icns_path], check=True)
        subprocess.run(["cp", icns_path, cmd_code_icns_path], check=True)
        print("  ✓ Generated icon.icns & Command Code.icns via iconutil")
        
    print("✨ Icon generation completed successfully!")

if __name__ == "__main__":
    main()
