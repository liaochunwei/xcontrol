#!/usr/bin/env swift
// scripts/macos/icon.swift — XControl macOS 应用图标生成器
//
// 依据：docs/06 §5.1（托盘图标 = 环形 + 中心点，品牌蓝 [45,105,220]）；
//       docs/10 §1.1（dmg.sh 组装 .app，图标经 CFBundleIconFile 引用）。
//
// 做什么：按 macOS 图标网格（圆角方块占画布 824/1024，圆角半径为边长的 22.37%）
// 绘制"白环 + 白点"图形于品牌蓝竖向渐变底，输出 iconset 全部 10 个尺寸的 PNG；
// 调用方随后 `iconutil -c icns` 合成 .icns。图形比例与 xcontrol-tray icons.rs
// 的 draw() 同源（环外径 0.44、内径 0.30、中心点 0.10，图形盒占方块边长 68%）。
//
// 不烘焙投影：Dock / Finder 会按标准圆角方块图标自行加阴影。
//
// 用法：swift scripts/macos/icon.swift <输出目录，须以 .iconset 结尾>
//       通常不经本脚本直接调用，而是 scripts/macos/icon.sh（生成 .icns）。

import Cocoa

let arguments = CommandLine.arguments
guard arguments.count == 2, arguments[1].hasSuffix(".iconset") else {
    FileHandle.standardError.write("usage: icon.swift <out.iconset>\n".data(using: .utf8)!)
    exit(2)
}
let outputDirectory = URL(fileURLWithPath: arguments[1])
try FileManager.default.createDirectory(at: outputDirectory, withIntermediateDirectories: true)

let colorSpace = CGColorSpaceCreateDeviceRGB()

func rgba(_ red: CGFloat, _ green: CGFloat, _ blue: CGFloat) -> CGColor {
    CGColor(colorSpace: colorSpace, components: [red / 255, green / 255, blue / 255, 1])!
}

/// 单一尺寸的图标位图（透明画布 + 圆角渐变方块 + 环点图形）。
func render(_ pixels: CGFloat) -> CGImage {
    let context = CGContext(
        data: nil, width: Int(pixels), height: Int(pixels), bitsPerComponent: 8,
        bytesPerRow: 0, space: colorSpace,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    )!
    context.clear(CGRect(x: 0, y: 0, width: pixels, height: pixels))

    let side = pixels * 824.0 / 1024.0
    let inset = (pixels - side) / 2.0
    let tile = CGRect(x: inset, y: inset, width: side, height: side)
    let corner = side * 0.2237

    context.saveGState()
    context.addPath(CGPath(roundedRect: tile, cornerWidth: corner, cornerHeight: corner, transform: nil))
    context.clip()

    // 品牌蓝竖向渐变（上浅下深）。
    let gradient = CGGradient(
        colorsSpace: colorSpace,
        colors: [rgba(72, 134, 238), rgba(26, 72, 166)] as CFArray,
        locations: [0, 1]
    )!
    context.drawLinearGradient(
        gradient, start: CGPoint(x: pixels / 2, y: pixels), end: CGPoint(x: pixels / 2, y: 0), options: []
    )

    let box = side * 0.68
    let center = CGPoint(x: pixels / 2, y: pixels / 2)
    context.setFillColor(rgba(255, 255, 255))
    context.setStrokeColor(rgba(255, 255, 255))
    context.setLineWidth(box * (0.44 - 0.30))
    context.addArc(
        center: center, radius: box * (0.44 + 0.30) / 2.0,
        startAngle: 0, endAngle: 2 * .pi, clockwise: false
    )
    context.strokePath()
    let dot = box * 0.10
    context.fillEllipse(in: CGRect(x: center.x - dot, y: center.y - dot, width: 2 * dot, height: 2 * dot))

    context.restoreGState()
    return context.makeImage()!
}

// iconutil 要求的 iconset 文件名表（10 个尺寸）。
let specs: [(name: String, pixels: CGFloat)] = [
    ("icon_16x16.png", 16), ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32), ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128), ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256), ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512), ("icon_512x512@2x.png", 1024),
]
for spec in specs {
    let bitmap = NSBitmapImageRep(cgImage: render(spec.pixels))
    try bitmap.representation(using: .png, properties: [:])!
        .write(to: outputDirectory.appendingPathComponent(spec.name))
}
print("iconset written: \(outputDirectory.path)")
