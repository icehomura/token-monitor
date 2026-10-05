<template>
  <section class="chart-box">
    <h2>RPM / 输出·输入·缓存词元</h2>
    <div ref="chartRef" class="chart"></div>
  </section>
</template>

<script setup>
import { ref, watch, onMounted, onBeforeUnmount, nextTick } from 'vue'
import { fmtTokens } from '../utils/format'
// echarts 按需导入，避免全量打包 ~1MB
import * as echarts from 'echarts/core'
import { BarChart, LineChart } from 'echarts/charts'
import {
  TitleComponent, TooltipComponent, GridComponent, LegendComponent, GraphicComponent,
} from 'echarts/components'
import { CanvasRenderer } from 'echarts/renderers'
import { themeColors, isLight, hexToRgba } from '../composables/useTheme'

echarts.use([
  BarChart, LineChart,
  TitleComponent, TooltipComponent, GridComponent, LegendComponent, GraphicComponent,
  CanvasRenderer,
])

const props = defineProps({
  labels: { type: Array, default: () => [] },
  rpms: { type: Array, default: () => [] },
  tpms: { type: Array, default: () => [] },
  inputTpms: { type: Array, default: () => [] },
  cachedTpms: { type: Array, default: () => [] },
  convertUnits: { type: Boolean, default: false },
})

const chartRef = ref(null)
let chart = null

function axis() {
  const tc = themeColors.value
  return {
    axisLine: { lineStyle: { color: tc.axis } },
    axisLabel: { color: tc.label, fontSize: 11 },
    splitLine: { lineStyle: { color: tc.split } },
  }
}

function tooltipStyle() {
  const light = isLight()
  const tc = themeColors.value
  return {
    backgroundColor: light ? 'rgba(255,255,255,.96)' : 'rgba(28,36,54,.96)',
    borderColor: light ? '#d7dee9' : '#263049',
    textStyle: { color: light ? '#2b3550' : '#dbe3f0', fontSize: 12 },
    axisPointer: {
      type: 'shadow',
      shadowStyle: { color: hexToRgba(tc.blue, 0.08) },
      lineStyle: { color: tc.axis },
    },
  }
}

function grad(c, top = 0.30) {
  return {
    color: {
      type: 'linear', x: 0, y: 0, x2: 0, y2: 1,
      colorStops: [
        { offset: 0, color: hexToRgba(c, top) },
        { offset: 1, color: hexToRgba(c, 0) },
      ],
    },
  }
}

function renderChart() {
  if (!chart) return
  const tc = themeColors.value

  // RPM 与三条词元线量级相差极大（RPM 个位数 / 输出词元千级 / 输入词元百万级），
  // 若共用坐标轴，小的会被压成一条直线；若归一化到同一峰值，刻度就成了假数。
  // 因此 RPM 左轴 + 三条词元线各占一条真实右轴，刻度 = tooltip = 原始值，不做任何缩放。
  // 轴名与数字列左对齐（数字左边缘 ≈ 轴位置 + 8）
  const GRID_RIGHT = 210, GRID_TOP = 48
  const W = chart.getWidth() || chartRef.value?.clientWidth || 1000
  const nameLeft = (offset) => (W - GRID_RIGHT + offset) + 8
  const axisNames = [
    { text: '输出', color: tc.green, offset: 0 },
    { text: '输入', color: tc.blue, offset: 70 },
    { text: '缓存', color: tc.cache, offset: 140 },
  ]
  const graphic = axisNames.map((n) => ({
    type: 'text',
    left: Math.round(nameLeft(n.offset)),
    top: GRID_TOP - 26,
    style: { text: n.text, fill: n.color, font: '11px "Microsoft YaHei", sans-serif' },
    silent: true,
  }))

  chart.setOption(
    {
      backgroundColor: 'transparent',
      tooltip: {
        trigger: 'axis',
        ...tooltipStyle(),
        formatter(params) {
          if (!params || !params.length) return ''
          let s = `<div style="font-size:12px;margin-bottom:4px">${params[0].axisValue}</div>`
          for (const p of params) {
            const color = p.color
            const unit = p.seriesName === 'RPM' ? '次/分' : '词元/分'
            // 数据未缩放，直接展示原始值
            const display = fmtTokens(Math.round(p.value) || 0, props.convertUnits)
            s += `<div style="display:flex;align-items:center;gap:6px;margin:2px 0">`
            s += `<span style="display:inline-block;width:8px;height:8px;border-radius:50%;background:${color}"></span>`
            s += `<span>${p.seriesName}：</span><b>${display}</b> <span style="color:#999">${unit}</span></div>`
          }
          return s
        },
      },
      legend: {
        data: ['RPM', '输出', '输入', '缓存'],
        top: 0,
        textStyle: { color: tc.label, fontSize: 12 },
      },
      // 右侧三条轴需要额外留白，故 right 比 left 大
      grid: { left: 56, right: 210, top: 48, bottom: 48 },
      graphic,
      xAxis: { type: 'category', data: props.labels, ...axis(), boundaryGap: true },
      yAxis: [
        {
          // 0：RPM，左侧
          type: 'value', name: 'RPM', position: 'left',
          nameGap: 10,
          nameTextStyle: { color: tc.label, align: 'left' },
          ...axis(),
          axisLabel: { ...axis().axisLabel, formatter: v => fmtTokens(v, props.convertUnits) },
          splitLine: { lineStyle: { color: tc.split } },
        },
        {
          // 1：输出词元，右侧靠内（graphic 标签替代轴名）
          type: 'value', position: 'right', offset: 0,
          ...axis(),
          axisLabel: { ...axis().axisLabel, color: tc.green, formatter: v => fmtTokens(v, props.convertUnits) },
          splitLine: { show: false },
        },
        {
          // 2：输入词元，右侧再向外偏移一条轴位
          type: 'value', position: 'right', offset: 70,
          ...axis(),
          axisLabel: { ...axis().axisLabel, color: tc.blue, formatter: v => fmtTokens(v, props.convertUnits) },
          splitLine: { show: false },
        },
        {
          // 3：缓存词元，最外侧
          type: 'value', position: 'right', offset: 140,
          ...axis(),
          axisLabel: { ...axis().axisLabel, color: tc.cache, formatter: v => fmtTokens(v, props.convertUnits) },
          splitLine: { show: false },
        },
      ],
      series: [
        {
          name: 'RPM', type: 'bar', yAxisIndex: 0, data: props.rpms,
          itemStyle: { color: 'rgba(124, 133, 152, 0.45)', borderRadius: [3, 3, 0, 0] },
          barMaxWidth: 26,
        },
        {
          name: '输出', type: 'line', yAxisIndex: 1, data: props.tpms,
          smooth: true, symbol: 'circle', symbolSize: 4,
          lineStyle: { color: tc.green, width: 2.4 },
          itemStyle: { color: tc.green },
          areaStyle: grad(tc.green),
        },
        {
          name: '输入', type: 'line', yAxisIndex: 2, data: props.inputTpms,
          smooth: true, symbol: 'circle', symbolSize: 4,
          lineStyle: { color: tc.blue, width: 2.4 },
          itemStyle: { color: tc.blue },
          areaStyle: grad(tc.blue),
        },
        {
          name: '缓存', type: 'line', yAxisIndex: 3, data: props.cachedTpms,
          smooth: true, symbol: 'circle', symbolSize: 3,
          lineStyle: { color: tc.cache, width: 1.6 },
          itemStyle: { color: tc.cache },
          areaStyle: grad(tc.cache, 0.10),
        },
      ],
    },
    { notMerge: true },
  )
}

watch(
  () =>
    `${props.labels.join('|')}|${props.rpms.join(',')}|${props.tpms.join(',')}|${props.inputTpms.join(',')}|${props.cachedTpms.join(',')}|${themeColors.value.blue}|${props.convertUnits}`,
  () => {
    nextTick(() => renderChart())
  },
)

onMounted(async () => {
  await nextTick()
  chart = echarts.init(chartRef.value)
  renderChart()
  // graphic 标签按像素宽度定位，首帧布局完成后再算一次
  requestAnimationFrame(() => renderChart())
  window.addEventListener('resize', () => { chart?.resize(); renderChart() })
  new ResizeObserver(() => { chart?.resize(); renderChart() }).observe(chartRef.value)
})

onBeforeUnmount(() => {
  chart?.dispose()
  chart = null
})

defineExpose({ renderChart })
</script>

<style scoped>
.chart-box {
  background: var(--panel); border: 1px solid var(--border);
  border-radius: 10px; padding: 14px;
  height: 100%;
  display: flex; flex-direction: column;
  overflow: hidden;
}
.chart-box h2 { font-size: 13px; color: var(--muted); font-weight: 500; margin-bottom: 8px; flex-shrink: 0; }
.chart { width: 100%; flex: 1; min-height: 0; }
</style>