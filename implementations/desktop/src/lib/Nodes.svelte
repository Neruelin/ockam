<script>
  import { invoke } from '@tauri-apps/api/tauri'
  let nodes= ''
  async function nodeList() {
    nodes = await invoke('node_list')
  }
</script>

<div>
  <button on:click="{nodeList}">List my nodes</button>
  <p>{nodes}</p>
</div>