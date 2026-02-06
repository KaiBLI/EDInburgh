<script setup lang="ts">
import type { Ref, ComputedRef } from 'vue'
import { ref, watch, type Ref, type ComputedRef } from 'vue'
import { storeToRefs } from 'pinia'
import { useStorage } from '@vueuse/core'
import { useBrowser } from './composables/browser'
import settings from '@/settings'

// WASM Bridge
import { EDI } from '../../wasm/pkg'

// Changed Decoder
import { MPEGDecoder } from '@eshaz/wasm-audio-decoders/tree/main/src/mpg123-decoder/dist.mpg123-decoder.js'

import type * as Types from '@/types'
import { useEDIStore } from '@/stores/edi'
import { usePlayerStore } from '@/stores/player'
import Panel from '@/components/ui/Panel.vue'
import Connection from '@/components/edi/connection/Connection.vue'
import Ensemble from '@/components/edi/ensemble/Ensemble.vue'
import ServiceTable from '@/components/edi/ensemble/ServiceTable.vue'
import ServiceDetail from '@/components/edi/service-detail/Service.vue'
import ServiceList from '@/components/edi/service-list/Services.vue'

import Settings from '@/components/settings/Settings.vue'
import EnsembleDiscovery from '@/components/directory/EnsembleDiscovery.vue'
import BrowserSupport from '@/components/browser/BrowserSupport.vue'
import Footer from '@/components/footer/Footer.vue'

const { browser } = useBrowser()

const resample = async (
  buffer: Float32Array,
  sourceRate: number,
  targetRate: number,
): Promise<Float32Array> => {
  if (sourceRate === targetRate) {
    return buffer 
  }
  const numFrames = buffer.length

  // Calculate target length
  const targetLength = Math.ceil((numFrames * targetRate) / sourceRate)

  // Create OfflineAudioContext
  const offlineContext = new OfflineAudioContext({
    numberOfChannels: 1,
    length: targetLength,
    sampleRate: targetRate,
  })

  // Create buffer in source rate
  const audioBuffer = offlineContext.createBuffer(1, numFrames, sourceRate)
  audioBuffer.copyToChannel(buffer, 0)

  // Buffer source
  const sourceNode = offlineContext.createBufferSource()
  sourceNode.buffer = audioBuffer
  sourceNode.connect(offlineContext.destination)
  sourceNode.start()

  // Render
  const resampledBuffer = await offlineContext.startRendering()

  return resampledBuffer.getChannelData(0)
}

interface Analyser {
  l: AnalyserNode
  r: AnalyserNode
}

class EDInburgh {
  edi: EDI
  ws: WebSocket | null = null
  audioContext: AudioContext | null = null
  workletNode: AudioWorkletNode | null = null
  gainNode: GainNode | null = null
  gainFadeNode: GainNode | null = null
  
  // DECODER VERIFY NAME !!!
  decoder: MPEGDecoder | null = null
  
  analyser: Analyser | null = null
  analyserReading = false
  decodeAudio: boolean = false
  volume: number = 0


  updateEnsemble: typeof useEDIStore.prototype.updateEnsemble
  updateDL: typeof useEDIStore.prototype.updateDL
  updateSLS: typeof useEDIStore.prototype.updateSLS
  selectService: typeof useEDIStore.prototype.selectService
  setAudioFormat: typeof useEDIStore.prototype.setAudioFormat
  setPlayerState: typeof usePlayerStore.prototype.setState

  
  connected: Ref<boolean>
  selectedService: ComputedRef<Types.Service | undefined>
  playerVolume: ComputedRef<number>

  level: Ref<Types.Level>

  constructor({
    updateEnsemble,
    updateDL,
    updateSLS,
    selectService,
    setAudioFormat,
    setPlayerState,
    //
    connected,
    selectedService,
    playerVolume,


  }: {
    updateEnsemble: typeof useEDIStore.prototype.updateEnsemble
    updateDL: typeof useEDIStore.prototype.updateDL
    updateSLS: typeof useEDIStore.prototype.updateSLS
    selectService: typeof useEDIStore.prototype.selectService
    setAudioFormat: typeof useEDIStore.prototype.setAudioFormat
    setPlayerState: typeof usePlayerStore.prototype.setState
    //
    connected: Ref<boolean>
    selectedService: ComputedRef<Types.Service | undefined>
    playerVolume: ComputedRef<number>
  }) { 
    console.log('EDInburgh:init')
        // pinia store mappings
    this.updateEnsemble = updateEnsemble
    this.updateDL = updateDL
    this.updateSLS = updateSLS
    this.selectService = selectService
    this.setAudioFormat = setAudioFormat
    this.setPlayerState = setPlayerState
    //
    this.connected = connected
    this.selectedService = selectedService
    this.playerVolume = playerVolume

    this.level = ref<Types.Level>({
      l: 0,
      r: 0,
    })

     /******************************************************************
     EDI Events / Callbacks
     ******************************************************************/


        const edi = new EDI()



    edi.addEventListener('ensemble_updated', async (e: CustomEvent) => {
      await this.updateEnsemble(e.detail as Types.Ensemble)
    })

    edi.addEventListener('mot_image', async (e: CustomEvent) => {
      // console.debug('EDInburgh: mot_image', e.detail)
      await this.updateSLS(e.detail as Types.SLS)
    })

    edi.addEventListener('dl_object', async (e: CustomEvent) => {
      await this.updateDL(e.detail as Types.DL)
    })

    /**
     * UPDATED: Listen for mp2_segment
     * The WASM EDI bridge emits mp2_segment for legacy audio.
     */
    edi.addEventListener('mp2_segment', async (e: CustomEvent) => {
      const segment = e.detail // Should contain .frames (Uint8Array chunks)
      
      if (!this.decodeAudio) return
      const selected = this.selectedService.value
      if (!selected || segment.scid !== selected.scid) return

      segment.frames.forEach((frame: Uint8Array) => {
        this.processMP2Frame(frame)
      })
    })

    this.edi = edi


    // Watch for selected SID changes
    watch(
      () => this.selectedService.value?.scid,
      async (newScid, oldScid) => {
        if (newScid !== oldScid) {
          await this.resetAudioDecoder(this.selectedService.value?.audioFormat)
          await this.startAnalyser()
          await this.fadeIn(0.2)
        }
        // this.decodeAudio = true
      },
      { immediate: true },
    )

    // Watch settings
    watch(
      () => this.playerVolume.value,
      async (val) => {
        this.volume = val
        if (this.gainNode) {
          this.gainNode.gain.value = val
        }
      },
      { immediate: true },
    )
  }

  private wsOnMessage = (event: MessageEvent) => {
    this.edi.feed(new Uint8Array(event.data))
  }

  private wsOnMClose = () => {
    console.info('WebSocket closed')
    this.connected.value = false
    this.ws = null
    this.wsReset()
  }

  private wsOnError = (e: Event) => {
    console.error('WebSocket error:', e)
  }

  private wsReset(): void {
    if (!this.ws) return
    this.ws.removeEventListener('message', this.wsOnMessage)
    this.ws.removeEventListener('close', this.wsOnMClose)
    this.ws.removeEventListener('error', this.wsOnError)
  }

  private getWsUri(host: string, port: number): string {
    const path = settings.FRAME_FORWARDER_ENDPOINT
    // in case of an absolute URL
    if (path.startsWith('ws://') || path.startsWith('wss://')) {
      return `${path}/${host}/${port}/`
    }

    // build ws url depending on protocol
    const isHttps = document.location.protocol === 'https:'
    return `${isHttps ? 'wss://' : 'ws://'}${document.location.host}${path}/${host}/${port}/`
  }

  async connect(conn: { host: string; port: number }): Promise<void> {
    // const uri = `ws://localhost:9000/ws/${conn.host}/${conn.port}/`

    const uri = this.getWsUri(conn.host, conn.port)

    console.log('EDInburgh:connect', conn.host, conn.port, uri)

    const ws = new WebSocket(uri)

    ws.binaryType = 'arraybuffer'

    ws.addEventListener('message', this.wsOnMessage)
    ws.addEventListener('close', this.wsOnMClose)
    ws.addEventListener('error', this.wsOnError)

    this.ws = ws
    this.connected.value = true

    if (!this.decoder) {
      await this.initializeAudioDecoder()
    }
  }

  async reset(): Promise<void> {
    console.log('EDInburgh:reset')

    if (this.ws) {
      this.wsReset()

      try {
        this.ws.close(1000, 'Client disconnecting')

        await new Promise<void>((resolve) => {
          this.ws!.addEventListener('close', () => resolve(), { once: true })
        })
      } catch (err) {
        console.warn('WebSocket close error:', err)
      }

      this.ws = null
    }

    await this.edi.reset()

    this.connected.value = false
  }

  async initializeAudioDecoder(): Promise<void> {
    if (this.decoder) return


    const audioContext = new AudioContext({ latencyHint: 'balanced', sampleRate: 48000 })
    await audioContext.audioWorklet.addModule('pcm-processor.js')

    const workletNode = new AudioWorkletNode(audioContext, 'pcm-processor', { outputChannelCount: [2] })
    
    // Analyser and Gain setup
    const splitter = audioContext.createChannelSplitter(2)
    const analyserL = audioContext.createAnalyser()
    const analyserR = audioContext.createAnalyser()
    workletNode.connect(splitter)
    splitter.connect(analyserL, 0)
    splitter.connect(analyserR, 1)

    const gainNode = audioContext.createGain()
    gainNode.gain.value = this.volume
    const gainFadeNode = audioContext.createGain()
    gainFadeNode.gain.setValueAtTime(0.0, audioContext.currentTime)
    
    workletNode.connect(gainNode)
    gainNode.connect(gainFadeNode)
    gainFadeNode.connect(audioContext.destination)

    // INITIALIZE MPG123
    this.decoder = new MPEGDecoder()
    await this.decoder.ready

    this.audioContext = audioContext
    this.workletNode = workletNode
    this.gainNode = gainNode
    this.gainFadeNode = gainFadeNode
    this.analyser = { l: analyserL, r: analyserR }
  }

  async resetAudioDecoder(): Promise<void> {
    if (!this.decoder || !this.workletNode) return
    this.setPlayerState('stopped')
    
    // mpg123-decoder reset
    await this.decoder.reset()

    this.workletNode.port.postMessage({ type: 'reset' })
  }

  /**
   * NEW: Process MP2 frames using mpg123-decoder
   */
  async processMP2Frame(frame: Uint8Array): Promise<void> {
    if (!this.decoder || !this.decodeAudio) return

    // mpg123-decoder returns decoded PCM directly
    const { channelData, samplesDecoded, sampleRate } = this.decoder.decode(frame)

    if (samplesDecoded > 0) {
      this.playDecodedMP2(channelData, sampleRate)
    }
  }

  async playDecodedMP2(pcmData: Float32Array[], sampleRate: number): Promise<void> {
    if (!this.workletNode || !this.audioContext) return

    let outL = pcmData[0]
    let outR = pcmData[1] || pcmData[0] // Fallback to mono if needed

    // Resample if DAB stream rate doesn't match browser context
    if (sampleRate !== this.audioContext.sampleRate) {
      outL = await resample(outL, sampleRate, this.audioContext.sampleRate)
      outR = await resample(outR, sampleRate, this.audioContext.sampleRate)
    }

    this.setPlayerState('playing')
    this.workletNode.port.postMessage({
      type: 'audio',
      samples: [outL, outR],
    })
  }

  async fadeTo(value: number = 1.0, time: number = 1.0): Promise<void> {
    if (!this.decoder || !this.gainFadeNode || !this.audioContext) {
      return
    }

    console.debug('EDInburgh: fade in', time)

    const startTime = this.audioContext.currentTime
    const endTime = startTime + time

    this.gainFadeNode.gain.cancelScheduledValues(startTime)
    this.gainFadeNode.gain.setValueAtTime(this.gainFadeNode.gain.value, startTime)
    this.gainFadeNode.gain.linearRampToValueAtTime(value, endTime)

    // Wait until the fade completes using wall clock
    const now = this.audioContext.currentTime
    const remaining = Math.max(0, endTime - now)
    await new Promise<void>((resolve) => {
      setTimeout(resolve, remaining * 1000)
    })
  }

  async fadeIn(time: number = 0.5): Promise<void> {
    return await this.fadeTo(1.0, time)
  }

  async fadeOut(time: number = 0.5): Promise<void> {
    return await this.fadeTo(0.0, time)
  }

  async playService(sid: number): Promise<void> {
    console.debug('EDInburgh: play service', sid)

    if (this.decodeAudio && sid === this.selectedService.value?.sid) {
      console.info('EDInburgh: already playing service', sid)
      return
    }

    if (this.decodeAudio) {
      await this.fadeOut(0.1)
    }
    this.selectService(sid)
    this.decodeAudio = true
  }

  async stopService(): Promise<void> {
    console.debug('EDInburgh: stop service')
    this.decodeAudio = false
    this.setPlayerState('stopped')
  }

  async startAnalyser(): Promise<void> {
    if (!this.analyser) {
      console.info('analyser not initialized')
      return
    }

    if (this.analyserReading) {
      console.info('analyser already reading')
      return
    }

    this.analyserReading = true
    this.analyserLoop()
  }

  analyserLoop = () => {
    if (!this.analyser || !this.analyserReading) {
      return
    }

    const { l, r } = this.analyser

    const bufferL = new Float32Array(l.fftSize)
    const bufferR = new Float32Array(r.fftSize)

    l.getFloatTimeDomainData(bufferL)
    r.getFloatTimeDomainData(bufferR)

    const rmsLength = 2048 

    const sliceL = bufferL.slice(bufferL.length - rmsLength)
    const sliceR = bufferR.slice(bufferR.length - rmsLength)

    const rmsL = Math.hypot(...sliceL) / Math.sqrt(rmsLength)
    const rmsR = Math.hypot(...sliceR) / Math.sqrt(rmsLength)

    this.level.value = {
      l: rmsL,
      r: rmsR,
    }

    requestAnimationFrame(this.analyserLoop)
  }
}

const ediStore = useEDIStore()

const playerStore = usePlayerStore()

const {
  updateEnsemble,
  updateDL,
  updateSLS,
  selectService,
  setAudioFormat,
  reset: resetStore,
} = ediStore

const { setState: setPlayerState } = playerStore

const { connected, selectedService } = storeToRefs(ediStore)

const { volume: playerVolume } = storeToRefs(playerStore)

const edinburgh = new EDInburgh({
  updateEnsemble,
  updateDL,
  updateSLS,
  selectService,
  setAudioFormat,
  setPlayerState,
  //
  connected,
  selectedService,
  playerVolume,
})

/*
const connect = async (conn: { host: string, port: number }) => {
  await edinburgh.connect(conn)
}
*/

const connect = edinburgh.connect.bind(edinburgh)

// const reset = edinburgh.reset.bind(edinburgh)

const reset = async () => {
  await edinburgh.reset()
  await resetStore()
}

const { ediHost, ediPort } = storeToRefs(useEDIStore())

const selectEnsemble = async (conn: { host: string; port: number }) => {
  await edinburgh.reset()
  await resetStore()
  ediHost.value = conn.host
  ediPort.value = conn.port
  await connect(conn)
}

// ui states - maybe place somewhere else ;)

// const { system, store } = useColorMode()

// const colorMode = computed(() => store.value === 'auto' ? system.value : store.value)

const serviceTableExpanded = useStorage('edi/ensemble/service-table/expanded', false)
const ensembleDiscoveryExpanded = useStorage('edi/ensemble/ensemble-discovery/expanded', false)

const toggleServiceTable = () => {
  ensembleDiscoveryExpanded.value = false
  serviceTableExpanded.value = !serviceTableExpanded.value
}
const toggleEnsembleDiscovery = () => {
  serviceTableExpanded.value = false
  ensembleDiscoveryExpanded.value = !ensembleDiscoveryExpanded.value
}
</script>

<template>
  <main>
    <Panel v-if="!browser.isSupported" class="browser-support" variant="warning">
      <BrowserSupport :browser="browser" />
    </Panel>
    <Panel class="header">
      <template #header>
        <Settings />
      </template>
      <Ensemble />
      <div>
        <Connection @connect="connect" @reset="reset" />
      </div>
      <template #sub-navigation>
        <div class="sub-navigation">
          <div @click.prevent="toggleServiceTable()" class="toggle">
            <span class="label">Service Table</span>
            <span v-if="serviceTableExpanded" class="icon icon--close">⌃</span>
            <span v-else class="icon icon--open">⌄</span>
          </div>
          <div @click.prevent="toggleEnsembleDiscovery()" class="toggle">
            <span class="label">Ensemble Discovery</span>
            <span v-if="ensembleDiscoveryExpanded" class="icon icon--close">⌃</span>
            <span v-else class="icon icon--open">⌄</span>
          </div>
        </div>
      </template>
      <template #sub-content>
        <ServiceTable v-if="serviceTableExpanded" @select="(sid) => edinburgh.playService(sid)" />
        <EnsembleDiscovery v-if="ensembleDiscoveryExpanded" @select="selectEnsemble" />
      </template>
    </Panel>

    <Panel class="service-detail">
      <ServiceDetail :level="edinburgh.level.value" />
    </Panel>

    <Panel class="service-list">
      <ServiceList
        @play="(sid) => edinburgh.playService(sid)"
        @select="(sid) => edinburgh.playService(sid)"
        @stop="() => edinburgh.stopService()"
      />
    </Panel>

    <Panel class="footer" variant="transparent">
      <Footer />
    </Panel>
  </main>
</template>

<style lang="scss" scoped>
main {
  width: 100%;
  height: 100vh;
  max-width: calc(1024px + 2rem);
  padding-left: 1rem;
  padding-right: 1rem;
  margin-inline: auto;
  display: flex;
  flex-direction: column;

  > .browser-support {
    margin-top: 20px;
  }

  > .header {
    display: grid;
    grid-template-columns: 1fr 324px;
    gap: 12px;

    margin-top: 20px;
    margin-bottom: 20px;
    padding: 8px;
    padding-bottom: 0;

    .settings {
      margin-top: -8px;
      border-bottom: 1px solid hsl(var(--c-fg));
    }

    .sub-navigation {
      display: flex;
      gap: 8px;
      padding: 0 8px;
      border-top: 1px solid hsl(var(--c-fg));
      justify-content: space-between;
      .toggle {
        display: flex;
        align-items: center;
        gap: 4px;
        height: 24px;
        cursor: pointer;

        > .label {
          font-family: var(--t-family-mono);
          font-size: var(--t-fs-s);
        }

        > .icon {
          font-family: var(--t-family-mono);
          &--open {
            margin-top: -9px;
          }
          &--close {
            margin-top: 5px;
          }
        }
      }
    }
  }

  .service-detail {
    margin-bottom: 20px;
    padding: 8px;
  }

  .service-list {
    flex-grow: 1;
    overflow-y: auto;
    margin-bottom: 16px;

    /* scrollbar */
    &::-webkit-scrollbar {
      width: 4px;
      background: hsl(var(--c-fg) / 0.5);
    }

    &::-webkit-scrollbar-thumb {
      background: hsl(var(--c-fg));
      border-radius: 0;
    }
  }

  > .footer {
    margin-bottom: 0.5rem;
  }
}

button {
  margin: 5px;
  padding: 8px 12px;
  cursor: pointer;
}
</style>
