<template>
  <section class="chart-box">
    <h2>RPM / 输入词元 / 输出词元</h2>
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
  TitleComponent, TooltipComponent, GridComponent, LegendComponent,
} from 'echarts/components'
import { CanvasRenderer } from 'echarts/renderers'
import { themeColors, isLight, hexToRgba } from '../composables/useTheme'

echarts.use([
  BarChart, LineChart,
  TitleComponent, TooltipComponent, GridComponent, LegendComponent,
  CanvasRenderer,
])

const props = defineProps({
  labels: { type: Array, default: () => [] },
  rpms: { type: Array, default: () => [] },
  tpms: { type: Array, default: () => [] },
  inputTpms: { type: Array, default: () => [] },
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

function renderChart() {
  if (!chart) return
  const tc = themeColors.value

  // 三条线量级相差极大（RPM 个位数 / 输出词元千级 / 输入词元百万级），
  // 若共用坐标轴，小的会被压成一条直线；若归一化到同一峰值，刻度就成了假数。
  // 因此每条线各占一条真实坐标轴，刻度 = tooltip = 原始值，不做任何缩放。
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
        data: ['RPM', '输入词元', '输出词元'],
        top: 0,
        textStyle: { color: tc.label, fontSize: 12 },
      },
      // 右侧两条轴需要额外留白，故 right 比 left 大
      grid: { left: 56, right: 150, top: 36, bottom: 38 },
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
          // 1：输入词元，右侧靠内
          type: 'value', name: '输入词元', position: 'right', offset: 0,
          nameGap: 6,
          nameTextStyle: { color: tc.label, align: 'left' },
          ...axis(),
          axisLabel: { ...axis().axisLabel, formatter: v => fmtTokens(v, props.convertUnits) },
          splitLine: { show: false },
        },
        {
          // 2：输出词元，右侧再向外偏移一条轴位
          type: 'value', name: '输出词元', position: 'right', offset: 72,
          nameGap: 6,
          nameTextStyle: { color: tc.label, align: 'left' },
          ...axis(),
          axisLabel: { ...axis().axisLabel, formatter: v => fmtTokens(v, props.convertUnits) },
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
          name: '输入词元', type: 'line', yAxisIndex: 1, data: props.inputTpms,
          smooth: true, symbol: 'circle', symbolSize: 4,
          lineStyle: { color: tc.blue, width: 2 },
          itemStyle: { color: tc.blue },
        },
        {
          name: '输出词元', type: 'line', yAxisIndex: 2, data: props.tpms,
          smooth: true, symbol: 'circle', symbolSize: 5,
          lineStyle: { color: tc.green, width: 2 },
          itemStyle: { color: tc.green },
          areaStyle: {
            color: {
              type: 'linear', x: 0, y: 0, x2: 0, y2: 1,
              colorStops: [
                { offset: 0, color: hexToRgba(tc.green, 0.28) },
                { offset: 1, color: hexToRgba(tc.green, 0) },
              ],
            },
          },
        },
      ],
    },
    { notMerge: true },
  )
}

watch(
  () =>
    `${props.labels.join('|')}|${props.rpms.join(',')}|${props.tpms.join(',')}|${props.inputTpms.join(',')}|${themeColors.value.blue}|${props.convertUnits}`,
  () => {
    nextTick(() => renderChart())
  },
)

onMounted(async () => {
  await nextTick()
  chart = echarts.init(chartRef.value)
  renderChart()
  window.addEventListener('resize', () => chart?.resize())
  new ResizeObserver(() => chart?.resize()).observe(chartRef.value)
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