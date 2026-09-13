#!/usr/bin/env python3
"""
Generate a distinct, premium Apple HIG-style squircle app icon for Command Code.
Motif: Mathematically continuous Apple Command Key (⌘) fused with an orbital quantum
routing halo and glowing neon bioluminescence.
Produces:
- src-tauri/icons/icon-1024.png (1024x1024)
- src-tauri/icons/icon.png (512x512)
- src-tauri/icons/128x128@2x.png (256x256)
- src-tauri/icons/128x128.png (128x128)
- src-tauri/icons/32x32.png (32x32)
- src-tauri/icons/icon.icns (via iconutil)
- src-tauri/icons/Command Code.icns
"""

import math
import os
import subprocess
import tempfile
from PIL import Image, ImageDraw, ImageFilter

SIZE = 1024

def make_command_mask(target_size, d, r, w, supersample=4):
    """
    Renders a mathematically continuous Apple Command symbol (⌘) mask at high resolution
    using constructive solid geometry (circles + rectangles minus inner holes),
    then downsamples with high quality Lanczos anti-aliasing.
    """
    S = target_size * supersample
    scale = supersample
    cx, cy = S // 2, S // 2
    
    sd = d * scale
    sr = r * scale
    sw = w * scale
    
    x1, x2 = cx - sd, cx + sd
    y1, y2 = cy - sd, cy + sd
    
    c_tl = (x1 - sr, y1 - sr)
    c_tr = (x2 + sr, y1 - sr)
    c_br = (x2 + sr, y2 + sr)
    c_bl = (x1 - sr, y2 + sr)
    
    mask = Image.new("L", (S, S), 0)
    draw = ImageDraw.Draw(mask)
    
    # 1. Four outer solid discs
    out_r = sr + sw // 2
    for c in [c_tl, c_tr, c_br, c_bl]:
        draw.ellipse([c[0] - out_r, c[1] - out_r, c[0] + out_r, c[1] + out_r], fill=255)
        
    # 2. Horizontal and vertical connecting bars
    half_w = sw // 2
    draw.rectangle([x1 - sr, y1 - half_w, x2 + sr, y1 + half_w], fill=255)
    draw.rectangle([x1 - sr, y2 - half_w, x2 + sr, y2 + half_w], fill=255)
    draw.rectangle([x1 - half_w, y1 - sr, x1 + half_w, y2 + sr], fill=255)
    draw.rectangle([x2 - half_w, y1 - sr, x2 + half_w, y2 + sr], fill=255)
    
    # 3. Subtract four inner holes
    in_r = sr - half_w
    for c in [c_tl, c_tr, c_br, c_bl]:
        draw.ellipse([c[0] - in_r, c[1] - in_r, c[0] + in_r, c[1] + in_r], fill=0)
        
    # 4. Subtract central square hole
    draw.rectangle([x1 + half_w, y1 + half_w, x2 - half_w, y2 - half_w], fill=0)
    
    return mask.resize((target_size, target_size), Image.Resampling.LANCZOS)

def create_gradient_layer(size, c1, c2):
    """Creates a diagonal linear gradient image."""
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    for y in range(size):
        for x in range(0, size, 4):
            # Diagonal gradient from top-left to bottom-right
            t = (x + y) / float(2 * size)
            r = int(c1[0] * (1 - t) + c2[0] * t)
            g = int(c1[1] * (1 - t) + c2[1] * t)
            b = int(c1[2] * (1 - t) + c2[2] * t)
            a = int(c1[3] * (1 - t) + c2[3] * t)
            draw.rectangle([x, y, x + 3, y], fill=(r, g, b, a))
    return img

def create_master_icon():
    img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    
    # 1. Base squircle bounds (macOS standard icon grid: 824x824 inside 1024x1024)
    left, top, right, bottom = 100, 100, 924, 924
    corner_radius = 185
    
    # Mask
    mask = Image.new("L", (SIZE, SIZE), 0)
    draw_mask = ImageDraw.Draw(mask)
    draw_mask.rounded_rectangle([left, top, right, bottom], radius=corner_radius, fill=255)
    
    # 2. Outer drop shadow
    shadow = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    for offset, blur, alpha in [(28, 38, 80), (14, 20, 100), (6, 8, 125)]:
        s = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
        sd = ImageDraw.Draw(s)
        sd.rounded_rectangle([left, top + offset, right, bottom + offset], radius=corner_radius, fill=(0, 0, 0, alpha))
        s = s.filter(ImageFilter.GaussianBlur(blur))
        shadow = Image.alpha_composite(shadow, s)
        
    img = Image.alpha_composite(img, shadow)

    # 3. Squircle body: Deep space obsidian metallic gradient
    body = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    body_draw = ImageDraw.Draw(body)
    
    for y in range(top, bottom):
        ratio = (y - top) / float(bottom - top)
        # Deep space dark slate: from top (28, 34, 52) to bottom (11, 15, 26)
        r = int(28 * (1 - ratio) + 11 * ratio)
        g = int(34 * (1 - ratio) + 15 * ratio)
        b = int(52 * (1 - ratio) + 26 * ratio)
        body_draw.line([(left, y), (right, y)], fill=(r, g, b, 255))
        
    # Ambient radial spotlight in center
    glow = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    glow_draw = ImageDraw.Draw(glow)
    cx, cy = SIZE // 2, SIZE // 2
    for rad in range(360, 0, -6):
        pct = 1.0 - (rad / 360.0)
        glow_draw.ellipse([cx - rad, cy - rad, cx + rad, cy + rad], fill=(14, 165, 233, int(45 * pct)))
    glow = glow.filter(ImageFilter.GaussianBlur(36))
    body = Image.alpha_composite(body, glow)

    # Chamfered metallic border and top specular rim light
    highlight = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    hl_draw = ImageDraw.Draw(highlight)
    hl_draw.rounded_rectangle([left + 1, top + 1, right - 1, bottom - 1], radius=corner_radius - 1, outline=(255, 255, 255, 50), width=3)
    hl_draw.rounded_rectangle([left + 4, top + 4, right - 4, bottom - 4], radius=corner_radius - 4, outline=(255, 255, 255, 18), width=1)
    hl_draw.arc([left + 2, top + 2, right - 2, top + 2 * corner_radius], start=190, end=350, fill=(255, 255, 255, 120), width=4)
    hl_draw.arc([left + 2, bottom - 2 * corner_radius, right - 2, bottom - 2], start=10, end=170, fill=(0, 0, 0, 120), width=5)
    body = Image.alpha_composite(body, highlight)

    body.putalpha(mask)
    img = Image.alpha_composite(img, body)

    # 4. Orbital Quantum Halo Ring (around the Command Key)
    orb_radius = 280
    ring_layer = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    ring_draw = ImageDraw.Draw(ring_layer)
    
    # Outer subtle frosted ring track
    ring_draw.ellipse([cx - orb_radius, cy - orb_radius, cx + orb_radius, cy + orb_radius], outline=(56, 189, 248, 50), width=6)
    
    # Dynamic glowing arcs on the orbital ring (Cyan -> Purple)
    ring_draw.arc([cx - orb_radius, cy - orb_radius, cx + orb_radius, cy + orb_radius], start=135, end=315, fill=(6, 182, 212, 180), width=12)
    ring_draw.arc([cx - orb_radius, cy - orb_radius, cx + orb_radius, cy + orb_radius], start=315, end=495, fill=(168, 85, 247, 180), width=12)
    
    # Orbital routing nodes (dots on the ring representing accounts)
    for angle_deg in [45, 135, 225, 315]:
        rad = math.radians(angle_deg)
        nx = cx + int(orb_radius * math.cos(rad))
        ny = cy + int(orb_radius * math.sin(rad))
        ring_draw.ellipse([nx - 13, ny - 13, nx + 13, ny + 13], fill=(255, 255, 255, 230))
        ring_draw.ellipse([nx - 7, ny - 7, nx + 7, ny + 7], fill=(56, 189, 248, 255))
        
    img = Image.alpha_composite(img, ring_layer)

    # 5. The Mathematical Glowing Apple Command Symbol (⌘)
    d = 72
    r = 70
    
    # Layer A: Deep Wide Bloom (w = 64, blur = 32)
    mask_bloom1 = make_command_mask(SIZE, d, r, w=64)
    grad1 = create_gradient_layer(SIZE, (6, 182, 212, 170), (192, 132, 252, 170))
    grad1.putalpha(mask_bloom1)
    grad1 = grad1.filter(ImageFilter.GaussianBlur(30))
    img = Image.alpha_composite(img, grad1)

    # Layer B: Medium Neon Glow (w = 44, blur = 10)
    mask_bloom2 = make_command_mask(SIZE, d, r, w=44)
    grad2 = create_gradient_layer(SIZE, (34, 211, 238, 210), (216, 180, 254, 210))
    grad2.putalpha(mask_bloom2)
    grad2 = grad2.filter(ImageFilter.GaussianBlur(9))
    img = Image.alpha_composite(img, grad2)

    # Layer C: Solid High-Definition Glass Tube Core (w = 32)
    mask_core = make_command_mask(SIZE, d, r, w=32)
    grad_core = create_gradient_layer(SIZE, (224, 247, 250, 255), (250, 245, 255, 255))
    grad_core.putalpha(mask_core)

    # Center Golden Quantum Core (radiating warm photon energy inside the square)
    core_glow = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    cg_draw = ImageDraw.Draw(core_glow)
    for rad_val in range(54, 0, -4):
        pct = 1.0 - (rad_val / 54.0)
        cg_draw.ellipse([cx - rad_val, cy - rad_val, cx + rad_val, cy + rad_val], fill=(245, 158, 11, int(210 * pct)))
    core_glow = core_glow.filter(ImageFilter.GaussianBlur(10))
    img = Image.alpha_composite(img, core_glow)
    
    # Composite core tube on top
    img = Image.alpha_composite(img, grad_core)
    
    # Center photon star
    photon = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    p_draw = ImageDraw.Draw(photon)
    p_draw.ellipse([cx - 15, cy - 15, cx + 15, cy + 15], fill=(255, 255, 255, 255))
    p_draw.ellipse([cx - 8, cy - 8, cx + 8, cy + 8], fill=(254, 243, 199, 255))
    img = Image.alpha_composite(img, photon)
    
    return img

def main():
    icons_dir = os.path.abspath("src-tauri/icons")
    os.makedirs(icons_dir, exist_ok=True)
    
    print("🎨 Rendering 1024x1024 master icon (Command Key ⌘ + Quantum Routing Halo)...")
    master = create_master_icon()
    
    master_path = os.path.join(icons_dir, "icon-1024.png")
    master.save(master_path, "PNG")
    print(f"  ✓ Saved {master_path}")
    
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
        print("  ✓ Generated icon.icns & Command Code.icns via macOS iconutil")
        
    # Clean up test mask if exists
    test_mask = os.path.join(icons_dir, "test_mask.png")
    if os.path.exists(test_mask):
        os.remove(test_mask)

    print("✨ Icon generation completed successfully!")

if __name__ == "__main__":
    main()
