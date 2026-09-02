// SPDX-License-Identifier: MIT
// Vendored from foliate-js (https://github.com/johnfactotum/foliate-js),
// Copyright (c) 2022 John Factotum. May be modified for sidle.

const wait = ms => new Promise(resolve => setTimeout(resolve, ms))

const debounce = (f, wait, immediate) => {
    let timeout
    return (...args) => {
        const later = () => {
            timeout = null
            if (!immediate) f(...args)
        }
        const callNow = immediate && !timeout
        if (timeout) clearTimeout(timeout)
        timeout = setTimeout(later, wait)
        if (callNow) f(...args)
    }
}

const lerp = (min, max, x) => x * (max - min) + min
const easeOutQuad = x => 1 - (1 - x) * (1 - x)
const animate = (a, b, duration, ease, render) => new Promise(resolve => {
    let start
    const step = now => {
        if (document.hidden) {
            render(lerp(a, b, 1))
            return resolve()
        }
        start ??= now
        const fraction = Math.min(1, (now - start) / duration)
        render(lerp(a, b, ease(fraction)))
        if (fraction < 1) requestAnimationFrame(step)
        else resolve()
    }
    if (document.hidden) {
        render(lerp(a, b, 1))
        return resolve()
    }
    requestAnimationFrame(step)
})

// collapsed range doesn't return client rects sometimes (or always?)
const uncollapse = range => {
    if (!range?.collapsed) return range
    const { endOffset, endContainer } = range
    if (endContainer.nodeType === 1) {
        const node = endContainer.childNodes[endOffset]
        if (node?.nodeType === 1) return node
        return endContainer
    }
    if (endOffset + 1 < endContainer.length) range.setEnd(endContainer, endOffset + 1)
    else if (endOffset > 1) range.setStart(endContainer, endOffset - 1)
    else return endContainer.parentNode
    return range
}

const makeRange = (doc, node, start, end = start) => {
    const range = doc.createRange()
    range.setStart(node, start)
    range.setEnd(node, end)
    return range
}

// use binary search to find an offset value in a text node
const bisectNode = (doc, node, cb, start = 0, end = node.nodeValue.length) => {
    if (end - start === 1) {
        const result = cb(makeRange(doc, node, start), makeRange(doc, node, end))
        return result < 0 ? start : end
    }
    const mid = Math.floor(start + (end - start) / 2)
    const result = cb(makeRange(doc, node, start, mid), makeRange(doc, node, mid, end))
    return result < 0 ? bisectNode(doc, node, cb, start, mid)
        : result > 0 ? bisectNode(doc, node, cb, mid, end) : mid
}

const { SHOW_ELEMENT, SHOW_TEXT, SHOW_CDATA_SECTION,
    FILTER_ACCEPT, FILTER_REJECT, FILTER_SKIP } = NodeFilter

const filter = SHOW_ELEMENT | SHOW_TEXT | SHOW_CDATA_SECTION

// needed cause there seems to be a bug in `getBoundingClientRect()` in Firefox
// where it fails to include rects that have zero width and non-zero height
// (CSSOM spec says "rectangles [...] of which the height or width is not zero")
const getBoundingClientRect = target => {
    let top = Infinity, right = -Infinity, left = Infinity, bottom = -Infinity
    for (const rect of target.getClientRects()) {
        left = Math.min(left, rect.left)
        top = Math.min(top, rect.top)
        right = Math.max(right, rect.right)
        bottom = Math.max(bottom, rect.bottom)
    }
    return new DOMRect(left, top, right - left, bottom - top)
}

const getVisibleRange = (doc, start, end, mapRect) => {
    // first get all visible nodes
    const acceptNode = node => {
        const name = node.localName?.toLowerCase()
        // ignore all scripts, styles, and their children
        if (name === 'script' || name === 'style') return FILTER_REJECT
        if (node.nodeType === 1) {
            const { left, right } = mapRect(node.getBoundingClientRect())
            // no need to check child nodes if it's completely out of view
            if (right < start || left > end) return FILTER_REJECT
            // elements must be completely in view to be considered visible
            if (left >= start && right <= end) return FILTER_ACCEPT
            // TODO: it should probably allow elements that do not contain text
        } else {
            // ignore empty text nodes
            if (!node.nodeValue?.trim()) return FILTER_SKIP
            // create range to get rect
            const range = doc.createRange()
            range.selectNodeContents(node)
            const { left, right } = mapRect(range.getBoundingClientRect())
            // it's visible if any part of it is in view
            if (right >= start && left <= end) return FILTER_ACCEPT
        }
        return FILTER_SKIP
    }
    const walker = doc.createTreeWalker(doc.body, filter, { acceptNode })
    const nodes = []
    for (let node = walker.nextNode(); node; node = walker.nextNode())
        nodes.push(node)

    // we're only interested in the first and last visible nodes
    const from = nodes[0] ?? doc.body
    const to = nodes[nodes.length - 1] ?? from

    // find the offset at which visibility changes
    const startOffset = from.nodeType === 1 ? 0
        : bisectNode(doc, from, (a, b) => {
            const p = mapRect(getBoundingClientRect(a))
            const q = mapRect(getBoundingClientRect(b))
            if (p.right < start && q.left > start) return 0
            return q.left > start ? -1 : 1
        })
    const endOffset = to.nodeType === 1 ? 0
        : bisectNode(doc, to, (a, b) => {
            const p = mapRect(getBoundingClientRect(a))
            const q = mapRect(getBoundingClientRect(b))
            if (p.right < end && q.left > end) return 0
            return q.left > end ? -1 : 1
        })

    const range = doc.createRange()
    range.setStart(from, startOffset)
    range.setEnd(to, endOffset)
    return range
}

const selectionIsBackward = sel => {
    const range = document.createRange()
    range.setStart(sel.anchorNode, sel.anchorOffset)
    range.setEnd(sel.focusNode, sel.focusOffset)
    return range.collapsed
}

const setSelectionTo = (target, collapse) => {
    let range
    if (target.startContainer) range = target.cloneRange()
    else if (target.nodeType) {
        range = document.createRange()
        range.selectNode(target)
    }
    if (range) {
        const sel = range.startContainer.ownerDocument.defaultView.getSelection()
        if (sel) {
            sel.removeAllRanges()
            if (collapse === -1) range.collapse(true)
            else if (collapse === 1) range.collapse()
            sel.addRange(range)
        }
    }
}

const getDirection = doc => {
    const { defaultView } = doc
    const { writingMode, direction } = defaultView.getComputedStyle(doc.body)
    const vertical = writingMode === 'vertical-rl'
        || writingMode === 'vertical-lr'
    const rtl = doc.body.dir === 'rtl'
        || direction === 'rtl'
        || doc.documentElement.dir === 'rtl'
    return { vertical, rtl }
}

const getBackground = doc => {
    const bodyStyle = doc.defaultView.getComputedStyle(doc.body)
    return bodyStyle.backgroundColor === 'rgba(0, 0, 0, 0)'
        && bodyStyle.backgroundImage === 'none'
        ? doc.defaultView.getComputedStyle(doc.documentElement).background
        : bodyStyle.background
}

const makeMarginals = (length, part) => Array.from({ length }, () => {
    const div = document.createElement('div')
    const child = document.createElement('div')
    div.append(child)
    child.setAttribute('part', part)
    return div
})

const setStylesImportant = (el, styles) => {
    const { style } = el
    for (const [k, v] of Object.entries(styles)) style.setProperty(k, v, 'important')
}

class View {
    #observer = new ResizeObserver(() => this.expand())
    #element = document.createElement('div')
    #iframe = document.createElement('iframe')
    #contentRange = document.createRange()
    #overlayer
    #vertical = false
    #rtl = false
    #column = true
    #size
    #layout = {}
    #expandKey // last-applied expand geometry; no-op echoes skip the writes
    #expandedSize // cached for #placeOverlayer (attach can precede any change)
    // In a two-page spread the secondary view is the left page: its strip
    // starts at the element's top (no leading blank page), and one shared
    // scroll offset shows page p in the primary and p+1 here.
    #secondary = false
    // A horizontal ltr-content section in an rtl book pages right to left:
    // the column order flips at the root while body text stays ltr.
    #bookRtl = false
    #flipDir = false
    constructor({ container, onExpand, secondary = false, bookRtl = false }) {
        this.container = container
        this.onExpand = onExpand
        this.#secondary = secondary
        this.#bookRtl = bookRtl
        this.#iframe.setAttribute('part', 'filter')
        this.#element.append(this.#iframe)
        Object.assign(this.#element.style, {
            boxSizing: 'content-box',
            // A secondary in normal flow displaces the primary's strip;
            // absolute from birth.
            position: secondary ? 'absolute' : 'relative',
            left: secondary ? '0' : '',
            top: secondary ? '0' : '',
            overflow: 'hidden',
            flex: '0 0 auto',
            width: '100%', height: '100%',
            display: 'flex',
            justifyContent: 'center',
            alignItems: secondary ? 'flex-start' : 'center',
            // Unsized (pre-expand()) the strip spans the whole container,
            // and a transparent box swallows clicks: no events yet.
            pointerEvents: secondary ? 'none' : '',
        })
        Object.assign(this.#iframe.style, {
            overflow: 'hidden',
            border: '0',
            display: 'none',
            width: '100%', height: '100%',
        })
        // `allow-scripts` is needed for events because of WebKit bug
        this.#iframe.setAttribute('sandbox', 'allow-same-origin allow-scripts')
        this.#iframe.setAttribute('scrolling', 'no')
    }
    get element() {
        return this.#element
    }
    get document() {
        return this.#iframe.contentDocument
    }
    async load(src, afterLoad, beforeRender) {
        if (typeof src !== 'string') throw new Error(`${src} is not string`)
        return new Promise(resolve => {
            this.#iframe.addEventListener('load', () => {
                const doc = this.document
                afterLoad?.(doc)

                // it needs to be visible for Firefox to get computed style
                this.#iframe.style.display = 'block'
                const { vertical, rtl } = getDirection(doc)
                const background = getBackground(doc)
                this.#iframe.style.display = 'none'

                this.#vertical = vertical
                this.#flipDir = !vertical && !rtl && this.#bookRtl
                this.#rtl = rtl || this.#flipDir

                // The document just swapped in (about:blank → section). Any
                this.#layout = {}
                this.#expandKey = undefined
                this.#expandedSize = undefined

                this.#contentRange.selectNodeContents(doc.body)
                const layout = beforeRender?.({ vertical, rtl: this.#rtl, background })
                this.#iframe.style.display = 'block'
                this.render(layout)
                this.#observer.observe(doc.body)

                // the resize observer above doesn't work in Firefox
                // (see https://bugzilla.mozilla.org/show_bug.cgi?id=1832939)
                doc.fonts.ready.then(() => this.expand())

                resolve()
            }, { once: true })
            this.#iframe.src = src
        })
    }
    render(layout, force) {
        if (!layout) return
        // Same geometry → nothing to do. Re-applied identical column styles
        // dirty style and force a full re-layout of the section (~200-900ms
        // for a long ruby-annotated vertical chapter).
        const last = this.#layout
        if (!force && ['flow', 'width', 'height', 'margin', 'gap', 'columnWidth', 'spread']
            .every(k => last[k] === layout[k])) return
        this.#column = layout.flow !== 'scrolled'
        this.#layout = layout
        if (this.#column) this.columnize(layout)
        else this.scrolled(layout)
    }
    scrolled({ gap, columnWidth }) {
        const vertical = this.#vertical
        const doc = this.document
        setStylesImportant(doc.documentElement, {
            'box-sizing': 'border-box',
            'padding': vertical ? `${gap}px 0` : `0 ${gap}px`,
            'column-width': 'auto',
            'height': 'auto',
            'width': 'auto',
        })
        setStylesImportant(doc.body, {
            [vertical ? 'max-height' : 'max-width']: `${columnWidth}px`,
            'margin': 'auto',
        })
        this.setImageSize()
        this.expand()
    }
    columnize({ width, height, gap, columnWidth }) {
        const vertical = this.#vertical
        this.#size = vertical ? height : width

        const doc = this.document
        setStylesImportant(doc.documentElement, {
            'box-sizing': 'border-box',
            'column-width': `${Math.trunc(columnWidth)}px`,
            'column-gap': `${gap}px`,
            'column-fill': 'auto',
            ...(vertical
                ? { 'width': `${width}px` }
                : { 'height': `${height}px` }),
            'padding': vertical ? `${gap / 2}px 0` : `0 ${gap / 2}px`,
            'overflow': 'hidden',
            // force wrap long words
            'overflow-wrap': 'break-word',
            // reset section-set props that conflict with the column styles
            'position': 'static', 'border': '0', 'margin': '0',
            'max-height': 'none', 'max-width': 'none',
            'min-height': 'none', 'min-width': 'none',
            // Line-box sizing at the column's overflow-hidden block edges.
            ...(vertical ? {} : { '-webkit-line-box-contain': 'block font replaced' }),
            // Root direction orders the column boxes; body direction below
            // keeps the text itself ltr.
            ...(this.#flipDir ? { 'direction': 'rtl' } : {}),
        })
        setStylesImportant(doc.body, {
            'max-height': 'none',
            'max-width': 'none',
            'margin': '0',
            ...(this.#flipDir ? { 'direction': 'ltr' } : {}),
        })
        this.setImageSize()
        this.expand()
    }
    setImageSize() {
        const { width, height, columnWidth } = this.#layout
        const doc = this.document
        // Layout geometry in physical px. Before the first layout these can be
        // 0/NaN — skip then (a later render() re-runs with real values).
        if (!(width > 0) || !(height > 0) || !(columnWidth > 0)) return
        // An image must fit ONE column box, not the page: on a two-column
        const vertical = this.#vertical
        const inlineCap = `${Math.trunc(columnWidth)}px`
        // The block axis is the one a column spans fully and along which content
        // fragments; its full extent is the page size on that axis.
        const blockExtent = vertical ? width : height
        // `[data-kfx-inline]` marks KFX render:inline glyph images (rare hanzi
        const els = [...doc.body.querySelectorAll('img:not([data-kfx-inline]), svg, video')]
        // READ pass: the block-axis space each image's wrapper chain consumes
        const view = doc.defaultView
        const [a, b] = vertical ? ['Left', 'Right'] : ['Top', 'Bottom']
        const px = v => parseFloat(v) || 0
        const chrome = els.map(el => {
            let sum = 0
            for (let n = el; n && n !== doc.documentElement; n = n.parentElement) {
                const cs = view.getComputedStyle(n)
                sum += px(cs[`margin${a}`]) + px(cs[`margin${b}`])
                if (n !== el) sum += px(cs[`padding${a}`]) + px(cs[`padding${b}`])
                    + px(cs[`border${a}Width`]) + px(cs[`border${b}Width`])
            }
            return sum
        })
        // WRITE pass.
        els.forEach((el, i) => {
            const blockMax = `${Math.max(0, blockExtent - chrome[i])}px`
            setStylesImportant(el, {
                // Both caps in definite px: a percentage cap resolves against
                // an indefinite page box. In vertical flow an author height
                // (`height: 100%`) yields to natural aspect; author width stands.
                ...(el.localName === 'img'
                    ? (vertical ? { 'height': 'auto' } : null)
                    : { 'width': 'auto', 'height': 'auto' }),
                'max-width': vertical ? blockMax : inlineCap,
                'max-height': vertical ? inlineCap : blockMax,
                'object-fit': 'contain',
                'page-break-inside': 'avoid',
                'break-inside': 'avoid',
                'box-sizing': 'border-box',
                // A baseline-aligned inline image reserves the line's descent
                'vertical-align': 'middle',
            })
        })
    }
    expand() {
        const { documentElement } = this.document
        if (this.#column) {
            const side = this.#vertical ? 'height' : 'width'
            const otherSide = this.#vertical ? 'width' : 'height'
            const contentRect = this.#contentRange.getBoundingClientRect()
            const rootRect = documentElement.getBoundingClientRect()
            // offset caused by column break at the start of the page
            const contentStart = this.#vertical ? 0
                : this.#rtl ? rootRect.right - contentRect.right : contentRect.left - rootRect.left
            const contentSize = contentStart + contentRect[side]
            const pageCount = Math.ceil(contentSize / this.#size)
            const expandedSize = pageCount * this.#size
            // In a spread the strip is one page wide, placed as the right
            // (primary) or left (secondary) half of the container.
            const spread = this.#layout.spread
            const otherSize = spread ? `${this.#layout.width}px` : '100%'
            if (spread) {
                Object.assign(this.#element.style, this.#secondary
                    ? { position: 'absolute', left: '0', marginLeft: '0', marginRight: '0', pointerEvents: '' }
                    : { position: 'relative', marginLeft: 'auto', marginRight: '0' })
            } else {
                Object.assign(this.#element.style,
                    { position: 'relative', marginLeft: '', marginRight: '' })
            }
            // Skip the size writes when nothing changed: writing identical
            const key = `col:${expandedSize}:${this.#size}:${otherSize}`
            if (this.#expandKey !== key) {
                this.#expandKey = key
                this.#expandedSize = expandedSize
                this.#element.style.padding = '0'
                this.#iframe.style[side] = `${expandedSize}px`
                this.#element.style[side] = `${expandedSize + this.#size * 2}px`
                this.#iframe.style[otherSide] = otherSize
                this.#element.style[otherSide] = otherSize
                documentElement.style[side] = `${this.#size}px`
            }
        } else {
            const side = this.#vertical ? 'width' : 'height'
            const otherSide = this.#vertical ? 'height' : 'width'
            const contentSize = documentElement.getBoundingClientRect()[side]
            const expandedSize = contentSize
            const { margin } = this.#layout
            const key = `scr:${expandedSize}:${margin}`
            if (this.#expandKey !== key) {
                this.#expandKey = key
                this.#expandedSize = expandedSize
                const padding = this.#vertical ? `0 ${margin}px` : `${margin}px 0`
                this.#element.style.padding = padding
                this.#iframe.style[side] = `${expandedSize}px`
                this.#element.style[side] = `${expandedSize}px`
                this.#iframe.style[otherSide] = '100%'
                this.#element.style[otherSide] = '100%'
            }
        }
        // Reposition + redraw the overlay on every expand, changed or not —
        this.#placeOverlayer()
        this.onExpand()
    }
    // Position + size the overlay SVG over the expanded view. Runs on every
    // expand and on attach — the attach-time call matters because expand()
    // may skip between attach and the next geometry change.
    #placeOverlayer() {
        if (!this.#overlayer || this.#expandedSize == null) return
        const expandedSize = this.#expandedSize
        if (this.#column) {
            const side = this.#vertical ? 'height' : 'width'
            const lead = this.#secondary ? 0 : this.#size
            this.#overlayer.element.style.margin = '0'
            this.#overlayer.element.style.left = this.#vertical ? '0' : `${lead}px`
            this.#overlayer.element.style.top = this.#vertical ? `${lead}px` : '0'
            this.#overlayer.element.style[side] = `${expandedSize}px`
        } else {
            const side = this.#vertical ? 'width' : 'height'
            const { margin } = this.#layout
            const padding = this.#vertical ? `0 ${margin}px` : `${margin}px 0`
            this.#overlayer.element.style.margin = padding
            this.#overlayer.element.style.left = '0'
            this.#overlayer.element.style.top = '0'
            this.#overlayer.element.style[side] = `${expandedSize}px`
        }
        this.#overlayer.redraw()
    }
    set overlayer(overlayer) {
        this.#overlayer = overlayer
        this.#element.append(overlayer.element)
        this.#placeOverlayer()
    }
    get overlayer() {
        return this.#overlayer
    }
    get contentPages() {
        return this.#expandedSize != null && this.#size > 0
            ? Math.round(this.#expandedSize / this.#size) : 0
    }
    get vertical() {
        return this.#vertical
    }
    destroy() {
        if (this.document) this.#observer.unobserve(this.document.body)
    }
}

// NOTE: everything here assumes the so-called "negative scroll type" for RTL
export class Paginator extends HTMLElement {
    static observedAttributes = [
        'flow', 'gap', 'margin',
        'max-inline-size', 'max-block-size', 'max-column-count',
        'vertical-spread', 'spread-gap',
    ]
    #root = this.attachShadow({ mode: 'closed' })
    #observer = new ResizeObserver(() => this.render())
    #top
    #background
    #container
    #header
    #footer
    #view
    // Two-page spread (vertical writing, wide window): the secondary View is
    // the left page, one page ahead of the primary on the shared scroll.
    #spreadView = null
    #spreadOn = false
    #spreadLoading = false
    #currentSrc = null
    #lastLayout = null
    // Cross-section fill: #crossView holds the next linear section, its
    // strip placed where the same-section strip ends — at the primary's
    // last page the left half shows that section's first page.
    #crossView = null
    #crossIndex = -1
    #crossSrc = null
    #crossLoading = false
    #vertical = false
    #rtl = false
    #margin = 0
    #index = -1
    #anchor = 0 // anchor view to a fraction (0-1), Range, or Element
    #justAnchored = false
    #locked = false // while true, prevent any further navigation
    #styles
    #styleMap = new WeakMap()
    #mediaQuery = matchMedia('(prefers-color-scheme: dark)')
    #mediaQueryListener
    #scrollBounds
    #touchState
    #touchScrolled
    #lastVisibleRange
    constructor() {
        super()
        this.#root.innerHTML = `<style>
        :host {
            display: block;
            container-type: size;
        }
        :host, #top {
            box-sizing: border-box;
            position: relative;
            overflow: hidden;
            width: 100%;
            height: 100%;
        }
        #top {
            --_gap: 7%;
            --_margin: 48px;
            --_max-inline-size: 720px;
            --_max-block-size: 1440px;
            --_max-column-count: 2;
            --_max-column-count-portrait: 1;
            --_max-column-count-spread: var(--_max-column-count);
            --_half-gap: calc(var(--_gap) / 2);
            --_max-width: calc(var(--_max-inline-size) * var(--_max-column-count-spread));
            --_max-height: var(--_max-block-size);
            display: grid;
            grid-template-columns:
                minmax(var(--_half-gap), 1fr)
                var(--_half-gap)
                minmax(0, calc(var(--_max-width) - var(--_gap)))
                var(--_half-gap)
                minmax(var(--_half-gap), 1fr);
            grid-template-rows:
                minmax(var(--_margin), 1fr)
                minmax(0, var(--_max-height))
                minmax(var(--_margin), 1fr);
            &.vertical {
                --_max-column-count-spread: var(--_max-column-count-portrait);
                --_max-width: var(--_max-block-size);
                --_max-height: calc(var(--_max-inline-size) * var(--_max-column-count-spread));
            }
            @container (orientation: portrait) {
                & {
                    --_max-column-count-spread: var(--_max-column-count-portrait);
                }
                &.vertical {
                    --_max-column-count-spread: var(--_max-column-count);
                }
            }
        }
        #background {
            grid-column: 1 / -1;
            grid-row: 1 / -1;
        }
        #container {
            grid-column: 2 / 5;
            grid-row: 2;
            position: relative;
            overflow: hidden;
        }
        :host([flow="scrolled"]) #container {
            grid-column: 1 / -1;
            grid-row: 1 / -1;
            overflow: auto;
        }
        #header {
            grid-column: 3 / 4;
            grid-row: 1;
        }
        #footer {
            grid-column: 3 / 4;
            grid-row: 3;
            align-self: end;
        }
        #header, #footer {
            display: grid;
            height: var(--_margin);
        }
        :is(#header, #footer) > * {
            display: flex;
            align-items: center;
            min-width: 0;
        }
        :is(#header, #footer) > * > * {
            width: 100%;
            overflow: hidden;
            white-space: nowrap;
            text-overflow: ellipsis;
            text-align: center;
            font-size: .75em;
            opacity: .6;
        }
        </style>
        <div id="top">
            <div id="background" part="filter"></div>
            <div id="header"></div>
            <div id="container"></div>
            <div id="footer"></div>
        </div>
        `

        this.#top = this.#root.getElementById('top')
        this.#background = this.#root.getElementById('background')
        this.#container = this.#root.getElementById('container')
        this.#header = this.#root.getElementById('header')
        this.#footer = this.#root.getElementById('footer')

        this.#observer.observe(this.#container)
        this.#container.addEventListener('scroll', () => this.dispatchEvent(new Event('scroll')))
        this.#container.addEventListener('scroll', debounce(() => {
            if (this.scrolled) {
                if (this.#justAnchored) this.#justAnchored = false
                else this.#afterScroll('scroll')
            }
        }, 250))

        const opts = { passive: false }
        this.addEventListener('touchstart', this.#onTouchStart.bind(this), opts)
        this.addEventListener('touchmove', this.#onTouchMove.bind(this), opts)
        this.addEventListener('touchend', this.#onTouchEnd.bind(this))
        this.addEventListener('load', ({ detail: { doc } }) => {
            doc.addEventListener('touchstart', this.#onTouchStart.bind(this), opts)
            doc.addEventListener('touchmove', this.#onTouchMove.bind(this), opts)
            doc.addEventListener('touchend', this.#onTouchEnd.bind(this))
        })

        this.addEventListener('relocate', ({ detail }) => {
            if (detail.reason === 'selection') setSelectionTo(this.#anchor, 0)
            else if (detail.reason === 'navigation') {
                if (this.#anchor === 1) setSelectionTo(detail.range, 1)
                else if (typeof this.#anchor === 'number')
                    setSelectionTo(detail.range, -1)
                else setSelectionTo(this.#anchor, -1)
            }
        })
        const checkPointerSelection = debounce((range, sel) => {
            if (!sel.rangeCount) return
            const selRange = sel.getRangeAt(0)
            const backward = selectionIsBackward(sel)
            if (backward && selRange.compareBoundaryPoints(Range.START_TO_START, range) < 0)
                this.prev()
            else if (!backward && selRange.compareBoundaryPoints(Range.END_TO_END, range) > 0)
                this.next()
        }, 700)
        this.addEventListener('load', ({ detail: { doc } }) => {
            let isPointerSelecting = false
            doc.addEventListener('pointerdown', () => isPointerSelecting = true)
            doc.addEventListener('pointerup', () => isPointerSelecting = false)
            let isKeyboardSelecting = false
            doc.addEventListener('keydown', () => isKeyboardSelecting = true)
            doc.addEventListener('keyup', () => isKeyboardSelecting = false)
            doc.addEventListener('selectionchange', () => {
                if (this.scrolled) return
                const range = this.#lastVisibleRange
                if (!range) return
                const sel = doc.getSelection()
                if (!sel.rangeCount) return
                // In a spread the two page docs are distinct; boundary
                // comparison against another doc's range throws.
                if (isPointerSelecting && sel.type === 'Range'
                    && range.startContainer.ownerDocument === doc)
                    checkPointerSelection(range, sel)
                else if (isKeyboardSelecting) {
                    const selRange = sel.getRangeAt(0).cloneRange()
                    const backward = selectionIsBackward(sel)
                    if (!backward) selRange.collapse()
                    this.#scrollToAnchor(selRange)
                }
            })
            doc.addEventListener('focusin', e => this.scrolled ? null :
                // NOTE: `requestAnimationFrame` is needed in WebKit
                requestAnimationFrame(() => this.#scrollToAnchor(e.target)))
        })

        this.#mediaQueryListener = () => {
            if (!this.#view) return
            this.#background.style.background = getBackground(this.#view.document)
        }
        this.#mediaQuery.addEventListener('change', this.#mediaQueryListener)
    }
    attributeChangedCallback(name, _, value) {
        switch (name) {
            case 'flow':
                this.render()
                break
            case 'gap':
            case 'margin':
            case 'max-block-size':
            case 'max-column-count':
                this.#top.style.setProperty('--_' + name, value)
                break
            case 'vertical-spread':
            case 'spread-gap':
                this.render()
                break
            case 'max-inline-size':
                // needs explicit `render()` as it doesn't necessarily resize
                this.#top.style.setProperty('--_' + name, value)
                this.render()
                break
        }
    }
    open(book) {
        this.bookDir = book.dir
        this.sections = book.sections
        book.transformTarget?.addEventListener('data', ({ detail }) => {
            if (detail.type !== 'text/css') return
            const w = innerWidth
            const h = innerHeight
            detail.data = Promise.resolve(detail.data).then(data => data
                // unprefix as most of the props are (only) supported unprefixed
                .replace(/(?<=[{\s;])-epub-/gi, '')
                // replace vw and vh as they cause problems with layout
                .replace(/(\d*\.?\d+)vw/gi, (_, d) => parseFloat(d) * w / 100 + 'px')
                .replace(/(\d*\.?\d+)vh/gi, (_, d) => parseFloat(d) * h / 100 + 'px')
                // `page-break-*` unsupported in columns; replace with `column-break-*`
                .replace(/page-break-(after|before|inside)\s*:/gi, (_, x) =>
                    `-webkit-column-break-${x}:`)
                .replace(/break-(after|before|inside)\s*:\s*(avoid-)?page/gi, (_, x, y) =>
                    `break-${x}: ${y ?? ''}column`))
        })
    }
    #destroyView() {
        this.#dropSpreadView()
        this.#dropCrossView()
        if (this.#view) {
            this.#view.destroy()
            this.#container.removeChild(this.#view.element)
            this.#view = null
        }
    }
    #createView() {
        this.#destroyView()
        this.#view = new View({
            container: this,
            bookRtl: this.bookDir === 'rtl',
            onExpand: () => {
                this.#positionCrossView()
                this.#scrollToAnchor(this.#anchor)
            },
        })
        this.#container.append(this.#view.element)
        return this.#view
    }
    #beforeRender({ vertical, rtl, background }) {
        this.#vertical = vertical
        this.#rtl = rtl
        this.#top.classList.toggle('vertical', vertical)

        // set background to `doc` background
        this.#background.style.background = background

        const { width, height } = this.#container.getBoundingClientRect()
        const size = vertical ? height : width

        const style = getComputedStyle(this.#top)
        let maxInlineSize = parseFloat(style.getPropertyValue('--_max-inline-size'))
        const maxColumnCount = parseInt(style.getPropertyValue('--_max-column-count-spread'))
        const margin = parseFloat(style.getPropertyValue('--_margin'))
        this.#margin = margin

        const g = parseFloat(style.getPropertyValue('--_gap')) / 100
        // The gap will be a percentage of the #container, not the whole view.
        const gap = -g / (g - 1) * size

        const spreadGap = Math.max(0, parseFloat(this.getAttribute('spread-gap')) || 0)
        const bookSpread = width > height
            && this.getAttribute('vertical-spread') === 'on'

        const flow = this.getAttribute('flow')
        if (flow === 'scrolled') {
            // FIXME: vertical-rl only, not -lr
            this.setAttribute('dir', vertical ? 'rtl' : 'ltr')
            this.#top.style.padding = '0'
            const columnWidth = maxInlineSize

            this.heads = null
            this.feet = null
            this.#header.replaceChildren()
            this.#footer.replaceChildren()

            this.#spreadOn = false
            this.#lastLayout = { flow, margin, gap, columnWidth }
            return this.#lastLayout
        }

        // A horizontal section inside a vertical book (title page, colophon)
        // reads as two facing half-width columns, never one window-wide one.
        if (!vertical && bookSpread)
            maxInlineSize = Math.min(maxInlineSize,
                Math.floor((width - spreadGap) / 2))

        const divisor = Math.min(maxColumnCount, Math.ceil(size / maxInlineSize))
        const columnWidth = (size / divisor) - gap
        this.setAttribute('dir', rtl ? 'rtl' : 'ltr')

        // Two side-by-side pages for vertical writing in a landscape window.
        // Vertical-rl column boxes stack top to bottom: the left page is a
        // second View on the shared scroll, never a second CSS column.
        const spread = vertical && bookSpread
        this.#spreadOn = spread
        const pageWidth = spread ? Math.floor((width - spreadGap) / 2) : width

        const marginalDivisor = vertical
            ? (spread ? 2 : Math.min(2, Math.ceil(width / maxInlineSize)))
            : divisor
        const marginalStyle = {
            gridTemplateColumns: `repeat(${marginalDivisor}, 1fr)`,
            gap: `${spread ? spreadGap : gap}px`,
            direction: this.bookDir === 'rtl' ? 'rtl' : 'ltr',
        }
        Object.assign(this.#header.style, marginalStyle)
        Object.assign(this.#footer.style, marginalStyle)
        const heads = makeMarginals(marginalDivisor, 'head')
        const feet = makeMarginals(marginalDivisor, 'foot')
        this.heads = heads.map(el => el.children[0])
        this.feet = feet.map(el => el.children[0])
        this.#header.replaceChildren(...heads)
        this.#footer.replaceChildren(...feet)

        this.#lastLayout = { height, width: pageWidth, margin, gap, columnWidth, spread }
        return this.#lastLayout
    }
    render(force) {
        if (!this.#view) return
        const layout = this.#beforeRender({
            vertical: this.#vertical,
            rtl: this.#rtl,
        })
        this.#view.render(layout, force === true)
        this.#syncSpreadView(layout, force === true)
        this.#scrollToAnchor(this.#anchor)
    }
    // Create, update, or drop the left-page Views to match `#spreadOn`.
    #syncSpreadView(layout, force) {
        if (!this.#spreadOn || this.scrolled || !this.#currentSrc) {
            this.#dropSpreadView()
            this.#dropCrossView()
            return
        }
        if (this.#spreadView) this.#spreadView.render(layout, force)
        else this.#loadSpreadView()
        this.#syncCrossView(layout, force)
    }
    #dropSpreadView() {
        if (!this.#spreadView) return
        this.#spreadView.destroy()
        this.#spreadView.element.remove()
        this.#spreadView = null
    }
    #syncCrossView(layout, force) {
        // nextIndex is null on the first linear section (it stands alone)
        // and when nothing linear follows.
        const nextIndex = this.#adjacentIndex(-1) == null
            ? null : this.#adjacentIndex(1)
        if (nextIndex == null) {
            this.#dropCrossView()
            return
        }
        if (this.#crossView && this.#crossIndex === nextIndex) {
            this.#crossView.render(layout, force)
            this.#positionCrossView()
            return
        }
        this.#dropCrossView()
        this.#loadCrossView(nextIndex)
    }
    #dropCrossView() {
        if (this.#crossView) {
            this.#crossView.destroy()
            this.#crossView.element.remove()
            this.#crossView = null
        }
        if (this.#crossSrc) {
            URL.revokeObjectURL(this.#crossSrc)
            this.#crossSrc = null
        }
        this.#crossIndex = -1
    }
    // #crossView's strip starts on the shared scroll where the primary's
    // content ends: its first page is one page after the primary's last.
    #positionCrossView() {
        if (!this.#crossView || !this.#view) return
        const pages = this.#view.contentPages
        if (pages > 0) this.#crossView.element.style.top = `${pages * this.size}px`
    }
    async #loadCrossView(index) {
        if (this.#crossLoading) return
        this.#crossLoading = true
        try {
            const forIndex = this.#index
            let src
            try {
                src = await Promise.resolve(this.sections[index].load())
            } catch (e) {
                console.warn(e)
                return
            }
            if (!this.#spreadOn || this.#index !== forIndex || this.#crossView) {
                URL.revokeObjectURL(src)
                return
            }
            const view = new View({
                container: this,
                onExpand: () => {},
                secondary: true,
                bookRtl: this.bookDir === 'rtl',
            })
            this.#container.append(view.element)
            const afterLoad = doc => {
                if (doc.head) {
                    const $styleBefore = doc.createElement('style')
                    doc.head.prepend($styleBefore)
                    const $style = doc.createElement('style')
                    doc.head.append($style)
                    this.#styleMap.set(doc, [$styleBefore, $style])
                }
                this.#writeStyles(doc)
                this.dispatchEvent(new CustomEvent('load', { detail: { doc, index } }))
            }
            try {
                await view.load(src, afterLoad, () => this.#lastLayout)
            } catch (e) {
                console.warn(e)
                view.element.remove()
                URL.revokeObjectURL(src)
                return
            }
            // A fill on another axis (a horizontal title page after a
            // vertical section) is dropped.
            if (!this.#spreadOn || this.#index !== forIndex || this.#crossView
                || view.vertical !== this.#vertical) {
                view.destroy()
                view.element.remove()
                URL.revokeObjectURL(src)
                return
            }
            this.dispatchEvent(new CustomEvent('create-overlayer', {
                detail: {
                    doc: view.document, index,
                    attach: overlayer => view.overlayer = overlayer,
                },
            }))
            this.#crossView = view
            this.#crossIndex = index
            this.#crossSrc = src
            this.#positionCrossView()
        } finally {
            this.#crossLoading = false
        }
    }
    async #loadSpreadView() {
        if (this.#spreadLoading) return
        this.#spreadLoading = true
        try {
            const src = this.#currentSrc
            const index = this.#index
            const view = new View({
                container: this,
                onExpand: () => {},
                secondary: true,
                bookRtl: this.bookDir === 'rtl',
            })
            this.#container.append(view.element)
            const afterLoad = doc => {
                if (doc.head) {
                    const $styleBefore = doc.createElement('style')
                    doc.head.prepend($styleBefore)
                    const $style = doc.createElement('style')
                    doc.head.append($style)
                    this.#styleMap.set(doc, [$styleBefore, $style])
                }
                this.#writeStyles(doc)
                this.dispatchEvent(new CustomEvent('load', { detail: { doc, index } }))
            }
            try {
                await view.load(src, afterLoad, () => this.#lastLayout)
            } catch (e) {
                console.warn(e)
                view.element.remove()
                return
            }
            // The section or the layout may have moved on while loading.
            if (src !== this.#currentSrc || !this.#spreadOn || this.#spreadView) {
                view.destroy()
                view.element.remove()
                return
            }
            this.dispatchEvent(new CustomEvent('create-overlayer', {
                detail: {
                    doc: view.document, index,
                    attach: overlayer => view.overlayer = overlayer,
                },
            }))
            this.#spreadView = view
        } finally {
            this.#spreadLoading = false
        }
    }
    get scrolled() {
        return this.getAttribute('flow') === 'scrolled'
    }
    get scrollProp() {
        const { scrolled } = this
        return this.#vertical ? (scrolled ? 'scrollLeft' : 'scrollTop')
            : scrolled ? 'scrollTop' : 'scrollLeft'
    }
    get sideProp() {
        const { scrolled } = this
        return this.#vertical ? (scrolled ? 'width' : 'height')
            : scrolled ? 'height' : 'width'
    }
    get size() {
        return this.#container.getBoundingClientRect()[this.sideProp]
    }
    get viewSize() {
        return this.#view.element.getBoundingClientRect()[this.sideProp]
    }
    get start() {
        return Math.abs(this.#container[this.scrollProp])
    }
    get end() {
        return this.start + this.size
    }
    get page() {
        return Math.floor(((this.start + this.end) / 2) / this.size)
    }
    get pages() {
        return Math.round(this.viewSize / this.size)
    }
    scrollBy(dx, dy) {
        const delta = this.#vertical ? dy : dx
        const element = this.#container
        const { scrollProp } = this
        const [offset, a, b] = this.#scrollBounds
        const rtl = this.#rtl
        const min = rtl ? offset - b : offset - a
        const max = rtl ? offset + a : offset + b
        element[scrollProp] = Math.max(min, Math.min(max,
            element[scrollProp] + delta))
    }
    snap(vx, vy) {
        const velocity = this.#vertical ? vy : vx
        const [offset, a, b] = this.#scrollBounds
        const { start, end, pages, size } = this
        const min = Math.abs(offset) - a
        const max = Math.abs(offset) + b
        const d = velocity * (this.#rtl ? -size : size)
        const page = Math.floor(
            Math.max(min, Math.min(max, (start + end) / 2
                + (isNaN(d) ? 0 : d))) / size)

        this.#scrollToPage(page, 'snap').then(() => {
            const dir = page <= 0 ? -1 : page >= pages - 1 ? 1 : null
            if (dir) return this.#goTo({
                index: this.#adjacentIndex(dir),
                anchor: dir < 0 ? () => 1 : () => 0,
            })
        })
    }
    #onTouchStart(e) {
        const touch = e.changedTouches[0]
        this.#touchState = {
            x: touch?.screenX, y: touch?.screenY,
            t: e.timeStamp,
            vx: 0, xy: 0,
        }
    }
    #onTouchMove(e) {
        const state = this.#touchState
        if (state.pinched) return
        state.pinched = globalThis.visualViewport.scale > 1
        if (this.scrolled || state.pinched) return
        if (e.touches.length > 1) {
            if (this.#touchScrolled) e.preventDefault()
            return
        }
        e.preventDefault()
        const touch = e.changedTouches[0]
        const x = touch.screenX, y = touch.screenY
        const dx = state.x - x, dy = state.y - y
        const dt = e.timeStamp - state.t
        state.x = x
        state.y = y
        state.t = e.timeStamp
        state.vx = dx / dt
        state.vy = dy / dt
        this.#touchScrolled = true
        this.scrollBy(dx, dy)
    }
    #onTouchEnd() {
        this.#touchScrolled = false
        if (this.scrolled) return

        // XXX: Firefox seems to report scale as 1... sometimes...?
        requestAnimationFrame(() => {
            if (globalThis.visualViewport.scale === 1)
                this.snap(this.#touchState.vx, this.#touchState.vy)
        })
    }
    // allows one to process rects as if they were LTR and horizontal
    #getRectMapper() {
        if (this.scrolled) {
            const size = this.viewSize
            const margin = this.#margin
            return this.#vertical
                ? ({ left, right }) =>
                    ({ left: size - right - margin, right: size - left - margin })
                : ({ top, bottom }) => ({ left: top + margin, right: bottom + margin })
        }
        const pxSize = this.pages * this.size
        return this.#rtl
            ? ({ left, right }) =>
                ({ left: pxSize - right, right: pxSize - left })
            : this.#vertical
                ? ({ top, bottom }) => ({ left: top, right: bottom })
                : f => f
    }
    async #scrollToRect(rect, reason) {
        if (this.scrolled) {
            const offset = this.#getRectMapper()(rect).left - this.#margin
            return this.#scrollTo(offset, reason)
        }
        const offset = this.#getRectMapper()(rect).left
        return this.#scrollToPage(Math.floor(offset / this.size) + (this.#rtl ? -1 : 1), reason)
    }
    async #scrollTo(offset, reason, smooth) {
        const element = this.#container
        const { scrollProp, size } = this
        if (element[scrollProp] === offset) {
            this.#scrollBounds = [offset, this.atStart ? 0 : size, this.atEnd ? 0 : size]
            this.#afterScroll(reason)
            return
        }
        // FIXME: vertical-rl only, not -lr
        if (this.scrolled && this.#vertical) offset = -offset
        if ((reason === 'snap' || smooth) && this.hasAttribute('animated')) return animate(
            element[scrollProp], offset, 300, easeOutQuad,
            x => element[scrollProp] = x,
        ).then(() => {
            this.#scrollBounds = [offset, this.atStart ? 0 : size, this.atEnd ? 0 : size]
            this.#afterScroll(reason)
        })
        else {
            element[scrollProp] = offset
            this.#scrollBounds = [offset, this.atStart ? 0 : size, this.atEnd ? 0 : size]
            this.#afterScroll(reason)
        }
    }
    async #scrollToPage(page, reason, smooth) {
        const offset = this.size * (this.#rtl ? -page : page)
        return this.#scrollTo(offset, reason, smooth)
    }
    async scrollToAnchor(anchor, select) {
        return this.#scrollToAnchor(anchor, select ? 'selection' : 'navigation')
    }
    async #scrollToAnchor(anchor, reason = 'anchor') {
        this.#anchor = anchor
        // anchor.page: an explicit page-number target
        if (anchor && typeof anchor === 'object' && typeof anchor.page === 'number') {
            await this.#scrollToPage(anchor.page, reason)
            return
        }
        const rects = uncollapse(anchor)?.getClientRects?.()
        // if anchor is an element or a range
        if (rects) {
            // when the start of the range is immediately after a hyphen in the
            const rect = Array.from(rects)
                .find(r => r.width > 0 && r.height > 0) || rects[0]
            if (!rect) return
            await this.#scrollToRect(rect, reason)
            return
        }
        // if anchor is a fraction
        if (this.scrolled) {
            await this.#scrollTo(anchor * this.viewSize, reason)
            return
        }
        const { pages } = this
        if (!pages) return
        const textPages = pages - 2
        const newPage = Math.round(anchor * (textPages - 1))
        await this.#scrollToPage(newPage + 1, reason)
    }
    #getVisibleRange() {
        if (this.scrolled) return getVisibleRange(this.#view.document,
            this.start + this.#margin, this.end - this.#margin, this.#getRectMapper())
        const size = this.#rtl ? -this.size : this.size
        // A spread shows two pages: the primary's page and the next one.
        const extra = this.#spreadOn ? this.size : 0
        return getVisibleRange(this.#view.document,
            this.start - size, this.end - size + extra, this.#getRectMapper())
    }
    #afterScroll(reason) {
        const range = this.#getVisibleRange()
        this.#lastVisibleRange = range
        // don't set new anchor if relocation was to scroll to anchor
        if (reason !== 'selection' && reason !== 'navigation' && reason !== 'anchor')
            this.#anchor = range
        else this.#justAnchored = true

        const index = this.#index
        const detail = { reason, range, index }
        if (this.scrolled) detail.fraction = this.start / this.viewSize
        else if (this.pages > 0) {
            const { page, pages } = this
            this.#header.style.visibility = page > 1 ? 'visible' : 'hidden'
            detail.fraction = (page - 1) / (pages - 2)
            detail.size = (this.#spreadOn ? 2 : 1) / (pages - 2)
        }
        this.dispatchEvent(new CustomEvent('relocate', { detail }))
    }
    async #display(promise) {
        const { index, src, anchor, onLoad, select } = await promise
        this.#index = index
        const hasFocus = this.#view?.document?.hasFocus()
        if (src) {
            // Drop the old view FIRST, then let the host set the incoming
            // section's layout attributes (margin / measure / column count)
            this.#destroyView()
            this.dispatchEvent(new CustomEvent('prerender', { detail: { index } }))
            const view = this.#createView()
            const afterLoad = doc => {
                if (doc.head) {
                    const $styleBefore = doc.createElement('style')
                    doc.head.prepend($styleBefore)
                    const $style = doc.createElement('style')
                    doc.head.append($style)
                    this.#styleMap.set(doc, [$styleBefore, $style])
                }
                onLoad?.({ doc, index })
            }
            const beforeRender = this.#beforeRender.bind(this)
            this.#currentSrc = src
            await view.load(src, afterLoad, beforeRender)
            this.dispatchEvent(new CustomEvent('create-overlayer', {
                detail: {
                    doc: view.document, index,
                    attach: overlayer => view.overlayer = overlayer,
                },
            }))
            this.#view = view
            this.#syncSpreadView(this.#lastLayout, false)
        }
        await this.scrollToAnchor((typeof anchor === 'function'
            ? anchor(this.#view.document) : anchor) ?? 0, select)
        if (hasFocus) this.focusView()
    }
    #canGoToIndex(index) {
        return index >= 0 && index <= this.sections.length - 1
    }
    async #goTo({ index, anchor, select}) {
        if (index === this.#index) await this.#display({ index, anchor, select })
        else {
            const oldIndex = this.#index
            const onLoad = detail => {
                this.sections[oldIndex]?.unload?.()
                this.setStyles(this.#styles)
                this.dispatchEvent(new CustomEvent('load', { detail }))
            }
            await this.#display(Promise.resolve(this.sections[index].load())
                .then(src => ({ index, src, anchor, onLoad, select }))
                .catch(e => {
                    console.warn(e)
                    console.warn(new Error(`Failed to load section ${index}`))
                    return {}
                }))
        }
    }
    async goTo(target) {
        if (this.#locked) return
        const resolved = await target
        if (this.#canGoToIndex(resolved.index)) return this.#goTo(resolved)
    }
    #scrollPrev(distance) {
        if (!this.#view) return true
        if (this.scrolled) {
            if (this.start > 0) return this.#scrollTo(
                Math.max(0, this.start - (distance ?? this.size)), null, true)
            return true
        }
        if (this.atStart) return
        const page = this.page - (this.#spreadOn ? 2 : 1)
        return this.#scrollToPage(page, 'page', true).then(() => page <= 0)
    }
    #scrollNext(distance) {
        if (!this.#view) return true
        if (this.scrolled) {
            if (this.viewSize - this.end > 2) return this.#scrollTo(
                Math.min(this.viewSize, distance ? this.start + distance : this.end), null, true)
            return true
        }
        if (this.atEnd) return
        const page = this.page + (this.#spreadOn ? 2 : 1)
        const pages = this.pages
        return this.#scrollToPage(page, 'page', true).then(() => page >= pages - 1)
    }
    get atStart() {
        return this.#adjacentIndex(-1) == null && this.page <= 1
    }
    get atEnd() {
        return this.#adjacentIndex(1) == null
            && this.page >= this.pages - (this.#spreadOn ? 3 : 2)
    }
    #adjacentIndex(dir, from = this.#index) {
        for (let index = from + dir; this.#canGoToIndex(index); index += dir)
            if (this.sections[index]?.linear !== 'no') return index
    }
    async #turnPage(dir, distance) {
        if (this.#locked) return
        this.#locked = true
        const prev = dir === -1
        // fromCross: the spread's left half held sections[crossIndex] page
        // 1 — the turn resumes that section at page 2, or passes a
        // single-page section entirely.
        const fromCross = !prev && this.#spreadOn && !this.scrolled
            && this.#crossView && this.page >= this.pages - 2
        const crossIndex = this.#crossIndex
        const crossPages = this.#crossView?.contentPages ?? 0
        const shouldGo = await (prev ? this.#scrollPrev(distance) : this.#scrollNext(distance))
        if (shouldGo) {
            let index = this.#adjacentIndex(dir)
            let anchor = prev ? () => 1 : () => 0
            if (fromCross && index === crossIndex) {
                if (crossPages <= 1) {
                    const beyond = this.#adjacentIndex(1, crossIndex)
                    if (beyond != null) index = beyond
                } else anchor = () => ({ page: 2 })
            }
            await this.#goTo({ index, anchor })
        }
        if (shouldGo || !this.hasAttribute('animated')) await wait(100)
        this.#locked = false
    }
    prev(distance) {
        return this.#turnPage(-1, distance)
    }
    next(distance) {
        return this.#turnPage(1, distance)
    }
    prevSection() {
        return this.goTo({ index: this.#adjacentIndex(-1) })
    }
    nextSection() {
        return this.goTo({ index: this.#adjacentIndex(1) })
    }
    firstSection() {
        const index = this.sections.findIndex(section => section.linear !== 'no')
        return this.goTo({ index })
    }
    lastSection() {
        const index = this.sections.findLastIndex(section => section.linear !== 'no')
        return this.goTo({ index })
    }
    getContents() {
        if (!this.#view) return []
        const contents = [{
            index: this.#index,
            overlayer: this.#view.overlayer,
            doc: this.#view.document,
        }]
        if (this.#spreadView) contents.push({
            index: this.#index,
            overlayer: this.#spreadView.overlayer,
            doc: this.#spreadView.document,
        })
        if (this.#crossView) contents.push({
            index: this.#crossIndex,
            overlayer: this.#crossView.overlayer,
            doc: this.#crossView.document,
        })
        return contents
    }
    #writeStyles(doc) {
        const $$styles = this.#styleMap.get(doc)
        if (!$$styles) return
        const [$beforeStyle, $style] = $$styles
        const styles = this.#styles
        if (Array.isArray(styles)) {
            const [beforeStyle, style] = styles
            $beforeStyle.textContent = beforeStyle
            $style.textContent = style
        } else $style.textContent = styles ?? ''
    }
    setStyles(styles) {
        this.#styles = styles
        if (!this.#styleMap.get(this.#view?.document)) return
        this.#writeStyles(this.#view.document)
        if (this.#spreadView) this.#writeStyles(this.#spreadView.document)
        if (this.#crossView) this.#writeStyles(this.#crossView.document)

        // NOTE: needs `requestAnimationFrame` in Chromium
        requestAnimationFrame(() =>
            this.#background.style.background = getBackground(this.#view.document))

        // needed because the resize observer doesn't work in Firefox
        this.#view?.document?.fonts?.ready?.then(() => this.#view.expand())
        this.#spreadView?.document?.fonts?.ready?.then(() => this.#spreadView?.expand())
    }
    focusView() {
        this.#view.document.defaultView.focus()
    }
    destroy() {
        this.#observer.unobserve(this)
        this.#dropSpreadView()
        this.#dropCrossView()
        this.#view.destroy()
        this.#view = null
        this.sections[this.#index]?.unload?.()
        this.#mediaQuery.removeEventListener('change', this.#mediaQueryListener)
    }
}

customElements.define('foliate-paginator', Paginator)
